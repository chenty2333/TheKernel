//! BTF objects and the small amount of type graph validation needed before a
//! BTF blob becomes a kernel object.  The bytes are retained verbatim: BTF is
//! an ABI object, not an eagerly translated debug format.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, Ordering};

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;

const BTF_MAGIC: u16 = 0xeb9f;
const BTF_VERSION: u8 = 1;
const BTF_HEADER_LEN: usize = 24;
const BTF_MAX_TYPE: u32 = 0x000f_ffff;
const BTF_KIND_INT: u32 = 1;
const BTF_KIND_PTR: u32 = 2;
const BTF_KIND_ARRAY: u32 = 3;
const BTF_KIND_STRUCT: u32 = 4;
const BTF_KIND_UNION: u32 = 5;
const BTF_KIND_ENUM: u32 = 6;
const BTF_KIND_FWD: u32 = 7;
const BTF_KIND_TYPEDEF: u32 = 8;
const BTF_KIND_VOLATILE: u32 = 9;
const BTF_KIND_CONST: u32 = 10;
const BTF_KIND_RESTRICT: u32 = 11;
const BTF_KIND_FUNC: u32 = 12;
const BTF_KIND_FUNC_PROTO: u32 = 13;
const BTF_KIND_VAR: u32 = 14;
const BTF_KIND_DATASEC: u32 = 15;
const BTF_KIND_FLOAT: u32 = 16;
const BTF_KIND_DECL_TAG: u32 = 17;
const BTF_KIND_TYPE_TAG: u32 = 18;
const BTF_KIND_ENUM64: u32 = 19;

pub struct BpfBtf {
    pub id: u32,
    pub bytes: Vec<u8>,
    pub type_count: u32,
}

/// The parser result is deliberately separate from publication.  BTF_LOAD
/// has user-visible verifier-log copyout after parsing, and a failed copyout
/// must not leave an ID reachable through the BTF registry.
pub struct ParsedBtf {
    bytes: Vec<u8>,
    type_count: u32,
    diagnostic: BtfDiagnostic,
}

/// The BTF input is capped at 16MiB.  A complete verifier stream consisting
/// of one header and one record per minimum-size type fits below this bound;
/// the cap is explicit and allocation failures are reported before publish.
pub struct BtfDiagnostic {
    bytes: VecDeque<u8>,
    total: u64,
    window: usize,
    fixed: bool,
    retention_lost: bool,
}

impl BtfDiagnostic {
    pub fn empty() -> Self {
        Self {
            bytes: VecDeque::new(),
            total: 0,
            window: 0,
            fixed: true,
            retention_lost: false,
        }
    }
    fn scratch() -> Self {
        Self {
            bytes: VecDeque::new(),
            total: 0,
            window: 256,
            fixed: true,
            retention_lost: false,
        }
    }
    pub fn for_log(log_size: u32, fixed: bool) -> Self {
        Self {
            bytes: VecDeque::new(),
            total: 0,
            window: log_size.saturating_sub(1) as usize,
            fixed,
            retention_lost: false,
        }
    }

    fn push(&mut self, byte: u8) -> AxResult {
        self.total = self.total.saturating_add(1);
        if self.window == 0 {
            return Ok(());
        }
        if self.bytes.len() < self.window {
            // The verifier log must never make a valid BTF load fail.  If a
            // user requests an impractically large window, retain the window
            // already obtained and continue counting exactly.
            if self.bytes.try_reserve(1).is_err() {
                self.retention_lost = true;
                self.window = self.bytes.len();
                return Ok(());
            }
            self.bytes.push_back(byte);
        } else if !self.fixed {
            self.bytes.pop_front();
            self.bytes.push_back(byte);
        }
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) -> AxResult {
        for &byte in bytes {
            self.push(byte)?;
        }
        Ok(())
    }

    fn decimal(&mut self, mut value: usize) -> AxResult {
        let mut digits = [0; 20];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (value % 10) as u8;
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        while count != 0 {
            count -= 1;
            self.push(digits[count])?;
        }
        Ok(())
    }
    fn extend_signed(&mut self, value: i64) -> AxResult {
        if value < 0 {
            self.push(b'-')?;
            self.decimal(value.unsigned_abs() as usize)
        } else {
            self.decimal(value as usize)
        }
    }
    fn hex(&mut self, mut value: u32) -> AxResult {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        self.extend(b"0x")?;
        let mut digits = [0u8; 8];
        let mut count = 0;
        loop {
            digits[count] = DIGITS[(value & 0xf) as usize];
            count += 1;
            value >>= 4;
            if value == 0 {
                break;
            }
        }
        while count != 0 {
            count -= 1;
            self.push(digits[count])?;
        }
        Ok(())
    }
    fn hex_bare(&mut self, mut value: u32) -> AxResult {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut digits = [0u8; 8];
        let mut count = 0;
        loop {
            digits[count] = DIGITS[(value & 0xf) as usize];
            count += 1;
            value >>= 4;
            if value == 0 {
                break;
            }
        }
        while count != 0 {
            count -= 1;
            self.push(digits[count])?;
        }
        Ok(())
    }

    fn header_with_total(&mut self, bytes: &[u8], total: usize) -> AxResult {
        let fields = [
            (
                b"magic: ".as_slice(),
                u16::from_le_bytes(bytes[0..2].try_into().unwrap()) as u32,
            ),
            (b"version: ".as_slice(), bytes[2] as u32),
            (b"flags: ".as_slice(), bytes[3] as u32),
            (b"hdr_len: ".as_slice(), u32_at(bytes, 4).unwrap()),
            (b"type_off: ".as_slice(), u32_at(bytes, 8).unwrap()),
            (b"type_len: ".as_slice(), u32_at(bytes, 12).unwrap()),
            (b"str_off: ".as_slice(), u32_at(bytes, 16).unwrap()),
            (b"str_len: ".as_slice(), u32_at(bytes, 20).unwrap()),
        ];
        for (name, value) in fields {
            self.extend(name)?;
            if name == b"magic: " || name == b"flags: " {
                self.hex(value)?;
            } else {
                self.decimal(value as usize)?;
            }
            self.push(b'\n')?;
        }
        self.extend(b"btf_total_size: ")?;
        self.decimal(total)?;
        self.push(b'\n')?;
        Ok(())
    }
    fn invalid_name_offset(&mut self, type_id: usize, offset: u32) -> AxResult {
        self.push(b'[')?;
        self.decimal(type_id)?;
        self.extend(b"] Invalid name_offset:")?;
        self.decimal(offset as usize)?;
        self.push(b'\n')
    }
    pub fn window_slices(&self) -> (&[u8], &[u8]) {
        self.bytes.as_slices()
    }
    pub fn window_len(&self) -> usize {
        self.bytes.len()
    }
    fn append_to(&self, target: &mut Self) -> AxResult {
        let prior_total = target.total;
        let (first, second) = self.window_slices();
        target.extend(first)?;
        target.extend(second)?;
        // The scratch diagnostic may retain only a prefix/suffix.  Preserve
        // its logical stream length and its inability-to-retain contract,
        // rather than treating retained bytes as the complete diagnostic.
        target.total = prior_total.saturating_add(self.total);
        target.retention_lost |= self.retention_lost;
        Ok(())
    }
    /// `bpf_vlog_finalize()` reports zero for a log to which no record was
    /// ever written; otherwise its ABI size includes the trailing NUL.
    pub fn true_size(&self) -> u32 {
        if self.retention_lost {
            // A successful parse with an incomplete verifier window would
            // falsely satisfy BTF_LOAD.  Force the normal ENOSPC protocol;
            // usercopy still runs first and can therefore return EFAULT.
            u32::MAX
        } else if self.total == 0 {
            0
        } else {
            self.total.saturating_add(1).min(u32::MAX as u64) as u32
        }
    }
}

static NEXT_BTF_ID: AtomicU32 = AtomicU32::new(1);
static BTF_IDS: SpinNoIrq<Vec<(u32, Weak<BpfBtf>)>> = SpinNoIrq::new(Vec::new());

/// Parse untrusted BTF without allocating a registry entry or assigning any
/// externally observable ID.  Callers retain the diagnostic on both paths.
pub fn parse(
    bytes: Vec<u8>,
    log_size: u32,
    fixed: bool,
) -> Result<ParsedBtf, (AxError, BtfDiagnostic)> {
    let mut diagnostic = BtfDiagnostic::for_log(log_size, fixed);
    match validate(&bytes) {
        Ok(sections) => {
            let (header, _) = match copy_btf_header(&bytes) {
                Ok(header) => header,
                Err(error) => return Err((error.errno(), diagnostic)),
            };
            match diagnostic.header_with_total(&header, bytes.len()) {
                Ok(()) => match validate_semantics(&bytes, sections, &mut diagnostic) {
                    Ok(type_count) => Ok(ParsedBtf {
                        bytes,
                        type_count,
                        diagnostic,
                    }),
                    Err(ValidationError::Semantic(error)) => Err((error.errno, diagnostic)),
                    Err(ValidationError::Parse(error)) => {
                        let errno = error.errno();
                        match error
                            .diagnostic()
                            .and_then(|failure| failure.append_to(&mut diagnostic))
                        {
                            Ok(()) => Err((errno, diagnostic)),
                            Err(error) => Err((error, BtfDiagnostic::for_log(log_size, fixed))),
                        }
                    }
                },
                Err(error) => Err((error, BtfDiagnostic::for_log(log_size, fixed))),
            }
        }
        Err(error) => {
            let errno = error.errno();
            let mut diagnostic = BtfDiagnostic::for_log(log_size, fixed);
            // btf_parse_hdr emits the parsed header before reporting an
            // unsupported version/flag or malformed post-header layout.
            // A short initial image cannot supply hdr_len.  Once it can, a
            // claimed hdr_len beyond data_size fails before any dump; every
            // dump is the zero-extended copied header, never raw input.
            if let Ok((header, _)) = copy_btf_header(&bytes) {
                if let Err(error) = diagnostic.header_with_total(&header, bytes.len()) {
                    return Err((error, BtfDiagnostic::for_log(log_size, fixed)));
                }
            }
            match error.diagnostic() {
                Ok(failure) => match failure.append_to(&mut diagnostic) {
                    Ok(()) => Err((errno, diagnostic)),
                    Err(error) => Err((error, BtfDiagnostic::for_log(log_size, fixed))),
                },
                Err(error) => Err((error, BtfDiagnostic::for_log(log_size, fixed))),
            }
        }
    }
}

impl ParsedBtf {
    pub fn diagnostic(&self) -> &BtfDiagnostic {
        &self.diagnostic
    }
}

/// Construct an object which has an ID but is not yet visible to ID lookup.
/// The caller can therefore reserve and fully prepare its first FD before
/// doing the final registry publication.
pub fn prepare(parsed: ParsedBtf) -> AxResult<Arc<BpfBtf>> {
    let id = NEXT_BTF_ID.fetch_add(1, Ordering::Relaxed);
    Arc::try_new(BpfBtf {
        id,
        bytes: parsed.bytes,
        type_count: parsed.type_count,
    })
    .map_err(|_| AxError::NoMemory)
}

/// Make a fully prepared BTF object visible to BTF ID operations.  Callers
/// must arrange that no fallible work remains after this point.
pub fn publish(object: &Arc<BpfBtf>) -> AxResult<()> {
    let mut ids = BTF_IDS.lock();
    ids.retain(|(_, object)| object.strong_count() != 0);
    ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    ids.push((object.id, Arc::downgrade(object)));
    Ok(())
}

pub fn by_id(id: u32) -> Option<Arc<BpfBtf>> {
    let mut ids = BTF_IDS.lock();
    ids.retain(|(_, object)| object.strong_count() != 0);
    ids.iter()
        .find(|(candidate, _)| *candidate == id)?
        .1
        .upgrade()
}

pub fn next_id(start: u32) -> Option<u32> {
    let mut ids = BTF_IDS.lock();
    ids.retain(|(_, object)| object.strong_count() != 0);
    ids.iter()
        .filter(|(id, _)| *id > start)
        .map(|(id, _)| *id)
        .min()
}

#[derive(Clone, Copy)]
enum BtfParseError {
    Header {
        message: &'static [u8],
        offset: usize,
    },
    UnsupportedHeader {
        offset: usize,
    },
    UnsupportedFeature {
        message: &'static [u8],
        offset: usize,
    },
    TooManyTypes {
        offset: usize,
    },
    NoTypeFound {
        offset: usize,
    },
    UnalignedTypeOffset {
        offset: usize,
    },
    Section {
        message: &'static [u8],
        offset: usize,
    },
    StringSectionInvalid {
        offset: usize,
    },
    NameOffsetInvalid {
        index: usize,
        offset: usize,
    },
    Type {
        message: &'static [u8],
        index: usize,
        offset: usize,
    },
    TypeMetaShort {
        index: usize,
        left: usize,
        needed: usize,
    },
}

impl BtfParseError {
    fn diagnostic(self) -> AxResult<BtfDiagnostic> {
        // btf_parse_hdr/btf_parse_type log verifier text directly; offsets
        // are not a generic suffix in the Linux stream.  Keep that staging
        // explicit here instead of reusing a synthetic parser formatter.
        if let Self::TypeMetaShort {
            index,
            left,
            needed,
        } = self
        {
            let mut log = BtfDiagnostic::scratch();
            log.push(b'[')?;
            log.decimal(index)?;
            log.extend(b"] meta_left:")?;
            log.decimal(left)?;
            log.extend(b" meta_needed:")?;
            log.decimal(needed)?;
            log.push(b'\n')?;
            return Ok(log);
        }
        let text: &'static [u8] = match self {
            Self::UnsupportedHeader { .. } => b"Unsupported btf_header",
            Self::UnsupportedFeature { message, .. } if message == b"unsupported BTF version" => {
                b"Unsupported version"
            }
            Self::UnsupportedFeature { .. } => b"Unsupported flags",
            Self::TooManyTypes { .. } => b"Exceeded max num of types",
            Self::NoTypeFound { .. } => b"No type found",
            Self::UnalignedTypeOffset { .. } => b"Unaligned type_off",
            Self::Header { message, .. } if message == b"missing header length" => {
                b"hdr_len not found"
            }
            Self::Header { message, .. } if message == b"invalid BTF magic" => b"Invalid magic",
            Self::Header { .. } => b"btf_header not found",
            Self::Section { message, .. } if message == b"type section is unaligned" => {
                b"Invalid section offset"
            }
            Self::Section { message, .. }
                if message == b"section range outside input"
                    || message == b"type length overflow"
                    || message == b"string length overflow" =>
            {
                b"Total section length too long"
            }
            Self::Section { .. } => b"Unsupported section found",
            Self::StringSectionInvalid { .. } => b"Invalid string section",
            Self::NameOffsetInvalid { .. } => b"Invalid name_offset",
            Self::Type { message, .. }
                if message == b"invalid type kind" || message == b"unsupported type kind" =>
            {
                b"Invalid kind"
            }
            Self::Type { message, .. } if message == b"type name offset has no string" => {
                b"Invalid name_offset"
            }
            Self::Type { .. } => b"Invalid type",
            Self::TypeMetaShort { .. } => unreachable!(),
        };
        let mut log = BtfDiagnostic::scratch();
        log.extend(text)?;
        log.push(b'\n')?;
        Ok(log)
    }
    fn errno(self) -> AxError {
        match self {
            Self::UnsupportedHeader { .. } | Self::TooManyTypes { .. } => {
                AxError::ArgumentListTooLong
            }
            Self::UnsupportedFeature { .. } => AxError::OperationNotSupported,
            _ => AxError::InvalidInput,
        }
    }
}

#[derive(Clone, Copy)]
struct TypeMeta {
    kind: u32,
    vlen: usize,
    kflag: bool,
    name: u32,
    size_or_type: u32,
    record: usize,
    payload: usize,
    payload_len: usize,
}
/// The verifier does not append a free-form error after a successful dump.
/// It stops at the record which made the BTF invalid.  Keep that ownership in
/// the error so delayed (graph-resolution) checks cannot accidentally report
/// the referred-to type instead of the offending member/argument.
#[derive(Clone, Copy)]
enum SemanticContext {
    Type(usize),
    Member { type_id: usize, member: usize },
    Vsi { type_id: usize, entry: usize },
    EnumValue { type_id: usize, value: usize },
}
/// A verifier diagnostic is selected from the v6.18 BTF diagnostic table;
/// semantic paths carry a reason code, never an arbitrary caller-owned
/// string.  The table retains byte spelling (and thus Linux's punctuation)
/// in one place for the log writer.
#[derive(Clone, Copy)]
enum LinuxReason {
    InvalidIntData(u32),
    IntVlenNonZero,
    TypeNonZero,
    InvalidBtfInfoKindFlag,
    SizeIsZero,
    IntBitsExceed128,
    IntBitsExceedTypeSize,
    UnsupportedEncoding,
    InvalidTypeId,
    InvalidName,
    InvalidValue,
    InvalidMemberNameOffset(u32),
    InvalidMemberBitsOffset,
    MemberBitsOffsetExceedsStructSize,
    InvalidMember,
    InvalidMemberOffset,
    MemberExceedsSize,
    InvalidReturnType,
    InvalidArg(usize),
    InvalidVsiType,
    InvalidVsiOffset,
    InvalidVsiSize,
    InvalidVsiOffsetPlusSize,
    InvalidVsiInfoSize,
    InvalidComponentIdx,
    InvalidNameOffset,
    InvalidInfo(u32),
    InvalidKind,
    InvalidTypeExact,
    InvalidIndex,
    InvalidAggregate,
    InvalidEnum,
    InvalidFuncLinkage,
    LinkageNotSupported,
    InvalidProto,
    InvalidDeclTag,
    InvalidPointer,
    InvalidArraySize,
    InvalidElem,
    InvalidArrayOfInt,
    ArraySizeOverflowsU32,
    TypeCycle,
    TypeDepth,
    SizeOverflow,
    SourceOnly,
    InternalNoMemory,
}
impl LinuxReason {
    fn append(self, log: &mut BtfDiagnostic) -> AxResult {
        match self {
            Self::InvalidIntData(data) => {
                log.extend(b"Invalid int_data:")?;
                log.hex_bare(data)
            }
            Self::IntVlenNonZero => log.extend(b"vlen != 0"),
            Self::TypeNonZero => log.extend(b"type != 0"),
            Self::InvalidBtfInfoKindFlag => log.extend(b"Invalid btf_info kind_flag"),
            Self::SizeIsZero => log.extend(b"size == 0"),
            Self::IntBitsExceed128 => log.extend(b"nr_bits exceeds 128"),
            Self::IntBitsExceedTypeSize => log.extend(b"nr_bits exceeds type_size"),
            Self::UnsupportedEncoding => log.extend(b"Unsupported encoding"),
            Self::InvalidTypeId => log.extend(b"Invalid type_id"),
            Self::InvalidName => log.extend(b"Invalid name"),
            Self::InvalidValue => log.extend(b"Invalid value"),
            Self::InvalidMemberNameOffset(offset) => {
                log.extend(b"Invalid member name_offset:")?;
                log.decimal(offset as usize)
            }
            Self::InvalidMemberBitsOffset => log.extend(b"Invalid member bits_offset"),
            Self::MemberBitsOffsetExceedsStructSize => {
                log.extend(b"Member bits_offset exceeds its struct size")
            }
            Self::InvalidMember => log.extend(b"Invalid member"),
            Self::InvalidMemberOffset => log.extend(b"Invalid member offset"),
            Self::MemberExceedsSize => log.extend(b"Member exceeds struct_size"),
            Self::InvalidReturnType => log.extend(b"Invalid return type"),
            Self::InvalidArg(arg) => {
                log.extend(b"Invalid arg#")?;
                log.decimal(arg)
            }
            Self::InvalidVsiType => log.extend(b"Invalid type_id"),
            Self::InvalidVsiOffset => log.extend(b"Invalid offset"),
            Self::InvalidVsiSize => log.extend(b"Invalid size"),
            Self::InvalidVsiOffsetPlusSize => log.extend(b"Invalid offset+size"),
            Self::InvalidVsiInfoSize => log.extend(b"Invalid btf_info size"),
            Self::InvalidComponentIdx => log.extend(b"Invalid component_idx"),
            Self::InvalidNameOffset => log.extend(b"Invalid name_offset"),
            Self::InvalidInfo(info) => {
                log.extend(b"Invalid btf_info:")?;
                log.hex_bare(info)
            }
            Self::InvalidKind => log.extend(b"Invalid kind"),
            Self::InvalidIndex => log.extend(b"Invalid index"),
            // This is intentionally not a fallback.  Each call site below
            // represents the verifier's literal `Invalid type` condition.
            Self::InvalidTypeExact => log.extend(b"Invalid type"),
            Self::InvalidAggregate => log.extend(b"Invalid aggregate"),
            Self::InvalidEnum => log.extend(b"Invalid enum"),
            Self::InvalidFuncLinkage => log.extend(b"Invalid func linkage"),
            Self::LinkageNotSupported => log.extend(b"Linkage not supported"),
            Self::InvalidProto => log.extend(b"Invalid FUNC_PROTO"),
            Self::InvalidDeclTag => log.extend(b"Invalid DECL_TAG"),
            Self::InvalidPointer => log.extend(b"Invalid pointer type"),
            Self::InvalidArraySize => log.extend(b"size != 0"),
            Self::InvalidElem => log.extend(b"Invalid elem"),
            Self::InvalidArrayOfInt => log.extend(b"Invalid array of int"),
            Self::ArraySizeOverflowsU32 => log.extend(b"Array size overflows U32_MAX"),
            Self::TypeCycle => log.extend(b"Invalid recursive type"),
            Self::TypeDepth => log.extend(b"Type depth exceeds limit"),
            Self::SizeOverflow => log.extend(b"Type size overflow"),
            Self::SourceOnly => log.extend(b"Invalid source-only type"),
            Self::InternalNoMemory => log.extend(b"BTF verifier allocation failed"),
        }
    }
}
#[derive(Clone, Copy)]
struct SemanticError {
    errno: AxError,
    reason: LinuxReason,
    index: usize,
    offset: usize,
    context: SemanticContext,
}
impl SemanticError {
    fn invalid(reason: LinuxReason, index: usize, offset: usize) -> Self {
        Self {
            errno: AxError::InvalidInput,
            reason,
            index,
            offset,
            context: SemanticContext::Type(index),
        }
    }
    fn unsupported(reason: LinuxReason, index: usize, offset: usize) -> Self {
        Self {
            errno: AxError::OperationNotSupported,
            reason,
            index,
            offset,
            context: SemanticContext::Type(index),
        }
    }
    fn invalid_arg(index: usize, parameter: usize, offset: usize) -> Self {
        Self {
            errno: AxError::InvalidInput,
            reason: LinuxReason::InvalidArg(parameter),
            index,
            offset,
            context: SemanticContext::Type(index),
        }
    }
    const fn with_context(mut self, context: SemanticContext) -> Self {
        self.context = context;
        self
    }
}

fn type_payload_len(kind: u32, vlen: usize) -> Option<usize> {
    Some(match kind {
        BTF_KIND_INT => 4,
        BTF_KIND_ARRAY => 12,
        BTF_KIND_STRUCT | BTF_KIND_UNION => vlen.checked_mul(12)?,
        BTF_KIND_ENUM => vlen.checked_mul(8)?,
        BTF_KIND_FUNC_PROTO => vlen.checked_mul(8)?,
        BTF_KIND_VAR => 4,
        BTF_KIND_DATASEC => vlen.checked_mul(12)?,
        BTF_KIND_DECL_TAG => 4,
        BTF_KIND_ENUM64 => vlen.checked_mul(12)?,
        BTF_KIND_PTR | BTF_KIND_FWD | BTF_KIND_TYPEDEF | BTF_KIND_VOLATILE | BTF_KIND_CONST
        | BTF_KIND_RESTRICT | BTF_KIND_FUNC | BTF_KIND_FLOAT | BTF_KIND_TYPE_TAG => 0,
        _ => return None,
    })
}

#[derive(Clone, Copy)]
struct BtfSections {
    type_start: usize,
    type_end: usize,
}

/// Decode exactly one record.  This is deliberately the only place which
/// advances a type cursor: callers can validate and log each accepted record
/// before exposing it to the graph resolver.
fn parse_one_type(
    bytes: &[u8],
    cursor: usize,
    type_end: usize,
    index: usize,
) -> Result<(TypeMeta, usize), BtfParseError> {
    let left = type_end.saturating_sub(cursor);
    if left < 12 {
        return Err(BtfParseError::TypeMetaShort {
            index,
            left,
            needed: 12,
        });
    }
    let name = u32_at(bytes, cursor).ok_or(BtfParseError::Type {
        message: b"missing type name offset",
        index,
        offset: cursor,
    })?;
    let info = u32_at(bytes, cursor + 4).ok_or(BtfParseError::Type {
        message: b"missing type info",
        index,
        offset: cursor + 4,
    })?;
    let kind = (info >> 24) & 0x1f;
    let vlen = (info & 0xffff) as usize;
    if kind == 0 || kind > BTF_KIND_ENUM64 {
        return Err(BtfParseError::Type {
            message: b"invalid type kind",
            index,
            offset: cursor + 4,
        });
    }
    let payload_len = type_payload_len(kind, vlen).ok_or(BtfParseError::Type {
        message: b"type payload length overflow",
        index,
        offset: cursor,
    })?;
    let payload = cursor.checked_add(12).ok_or(BtfParseError::Type {
        message: b"type payload offset overflow",
        index,
        offset: cursor,
    })?;
    let next = payload
        .checked_add(payload_len)
        .ok_or(BtfParseError::Type {
            message: b"type payload offset overflow",
            index,
            offset: cursor,
        })?;
    if next > type_end {
        return Err(BtfParseError::TypeMetaShort {
            index,
            left: type_end.saturating_sub(payload),
            needed: payload_len,
        });
    }
    let size_or_type = u32_at(bytes, cursor + 8).ok_or(BtfParseError::Type {
        message: b"missing type size_or_type",
        index,
        offset: cursor + 8,
    })?;
    Ok((
        TypeMeta {
            kind,
            vlen,
            kflag: info >> 31 != 0,
            name,
            size_or_type,
            record: cursor,
            payload,
            payload_len,
        },
        next,
    ))
}

fn type_at(types: &[TypeMeta], id: u32) -> Option<&TypeMeta> {
    if id == 0 {
        None
    } else {
        types.get(id as usize - 1)
    }
}

fn regular_int(bytes: &[u8], meta: TypeMeta) -> bool {
    if meta.kind != BTF_KIND_INT {
        return false;
    }
    let Some(data) = u32_at(bytes, meta.payload) else {
        return false;
    };
    let offset = (data >> 16) & 0xff;
    let bits = data & 0xff;
    offset == 0 && bits != 0 && bits.is_power_of_two() && bits <= 128 && bits % 8 == 0
}
/// Offsets into BTF's string section are byte offsets, not Rust strings.  In
/// particular this keeps malformed UTF-8 from accidentally being accepted by
/// a lossy conversion before the ABI-specific name checks below.
struct StringTable<'a> {
    bytes: &'a [u8],
    start: usize,
}
impl<'a> StringTable<'a> {
    fn from_btf(bytes: &'a [u8]) -> Self {
        let hdr = u32_at(bytes, 4).unwrap() as usize;
        let off = u32_at(bytes, 16).unwrap() as usize;
        let len = u32_at(bytes, 20).unwrap() as usize;
        Self {
            bytes: &bytes[hdr + off..hdr + off + len],
            start: hdr + off,
        }
    }
    fn string(&self, off: u32) -> Option<&'a [u8]> {
        let off = off as usize;
        let tail = self.bytes.get(off..)?;
        let end = tail.iter().position(|byte| *byte == 0)?;
        Some(&tail[..end])
    }
    fn nonempty(&self, off: u32) -> bool {
        self.string(off).is_some_and(|text| !text.is_empty())
    }
    fn valid_identifier(&self, off: u32) -> bool {
        let Some(text) = self.string(off) else {
            return false;
        };
        let Some((&first, rest)) = text.split_first() else {
            return false;
        };
        text.len() < 128
            && (first == b'_' || first == b'.' || first.is_ascii_alphabetic())
            && rest
                .iter()
                .all(|byte| *byte == b'_' || *byte == b'.' || byte.is_ascii_alphanumeric())
    }
    // ELF BTF section names are deliberately less restrictive than C names:
    // `.data`, `.rodata.str1.1`, and compiler-private names are all valid.
    fn valid_section(&self, off: u32) -> bool {
        self.string(off).is_some_and(|text| {
            text.len() < 128
                && !text.is_empty()
                && text
                    .iter()
                    .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
        })
    }
    fn offset(&self, off: u32) -> usize {
        self.start + off as usize
    }
}

fn kind_name(kind: u32) -> &'static [u8] {
    match kind {
        BTF_KIND_INT => b"INT",
        BTF_KIND_PTR => b"PTR",
        BTF_KIND_ARRAY => b"ARRAY",
        BTF_KIND_STRUCT => b"STRUCT",
        BTF_KIND_UNION => b"UNION",
        BTF_KIND_ENUM => b"ENUM",
        BTF_KIND_FWD => b"FWD",
        BTF_KIND_TYPEDEF => b"TYPEDEF",
        BTF_KIND_VOLATILE => b"VOLATILE",
        BTF_KIND_CONST => b"CONST",
        BTF_KIND_RESTRICT => b"RESTRICT",
        BTF_KIND_FUNC => b"FUNC",
        BTF_KIND_FUNC_PROTO => b"FUNC_PROTO",
        BTF_KIND_VAR => b"VAR",
        BTF_KIND_DATASEC => b"DATASEC",
        BTF_KIND_FLOAT => b"FLOAT",
        BTF_KIND_DECL_TAG => b"DECL_TAG",
        BTF_KIND_TYPE_TAG => b"TYPE_TAG",
        BTF_KIND_ENUM64 => b"ENUM64",
        _ => b"UNKNOWN",
    }
}
fn log_name(log: &mut BtfDiagnostic, strings: &StringTable<'_>, offset: u32) -> AxResult {
    if offset == 0 {
        return log.extend(b"(anon)");
    }
    log.extend(strings.string(offset).unwrap_or(b"(invalid-name)"))
}
/// This intentionally mirrors the verifier's per-kind stream instead of
/// fabricating a successful line for every type before validation completes.
fn log_type(
    log: &mut BtfDiagnostic,
    bytes: &[u8],
    strings: &StringTable<'_>,
    index: usize,
    meta: TypeMeta,
    reason: Option<LinuxReason>,
) -> AxResult {
    log.extend(b"[")?;
    log.decimal(index)?;
    log.extend(b"] ")?;
    log.extend(kind_name(meta.kind))?;
    log.extend(b" ")?;
    log_name(log, strings, meta.name)?;
    match meta.kind {
        BTF_KIND_INT if matches!(reason, Some(LinuxReason::InvalidIntData(_))) => {}
        BTF_KIND_INT => {
            let data = u32_at(bytes, meta.payload).unwrap();
            let enc = match (data >> 24) & 0xf {
                0 => b"(none)".as_slice(),
                1 => b"SIGNED".as_slice(),
                2 => b"CHAR".as_slice(),
                4 => b"BOOL".as_slice(),
                _ => b"UNKN".as_slice(),
            };
            log.extend(b" size=")?;
            log.decimal(meta.size_or_type as usize)?;
            log.extend(b" bits_offset=")?;
            log.decimal(((data >> 16) & 0xff) as usize)?;
            log.extend(b" nr_bits=")?;
            log.decimal((data & 0xff) as usize)?;
            log.extend(b" encoding=")?;
            log.extend(enc)?;
        }
        BTF_KIND_ARRAY => {
            log.extend(b" type_id=")?;
            log.decimal(u32_at(bytes, meta.payload).unwrap() as usize)?;
            log.extend(b" index_type_id=")?;
            log.decimal(u32_at(bytes, meta.payload + 4).unwrap() as usize)?;
            log.extend(b" nr_elems=")?;
            log.decimal(u32_at(bytes, meta.payload + 8).unwrap() as usize)?;
        }
        BTF_KIND_STRUCT | BTF_KIND_UNION | BTF_KIND_ENUM | BTF_KIND_ENUM64 | BTF_KIND_DATASEC => {
            log.extend(b" size=")?;
            log.decimal(meta.size_or_type as usize)?;
            log.extend(b" vlen=")?;
            log.decimal(meta.vlen)?;
        }
        BTF_KIND_FWD => {
            let style: &[u8] = if meta.kflag { b" union" } else { b" struct" };
            log.extend(style)?;
        }
        BTF_KIND_FUNC | BTF_KIND_PTR | BTF_KIND_TYPEDEF | BTF_KIND_VOLATILE | BTF_KIND_CONST
        | BTF_KIND_RESTRICT | BTF_KIND_TYPE_TAG => {
            log.extend(b" type_id=")?;
            log.decimal(meta.size_or_type as usize)?;
        }
        BTF_KIND_FUNC_PROTO => {
            log.extend(b" return=")?;
            log.decimal(meta.size_or_type as usize)?;
            log.extend(b" args=(")?;
            if meta.vlen == 0 {
                log.extend(b"void")?;
            } else {
                let first = meta.payload;
                let first_type = u32_at(bytes, first + 4).unwrap();
                if meta.vlen == 1 && first_type == 0 {
                    log.extend(b"vararg")?;
                } else {
                    for parameter in 0..meta.vlen {
                        let at = meta.payload + parameter * 8;
                        let ty = u32_at(bytes, at + 4).unwrap();
                        if parameter != 0 {
                            log.extend(b", ")?;
                        }
                        if ty == 0 {
                            log.extend(b"vararg")?;
                        } else {
                            log.decimal(ty as usize)?;
                            log.push(b' ')?;
                            log_name(log, strings, u32_at(bytes, at).unwrap())?;
                        }
                    }
                }
            }
            log.push(b')')?;
        }
        BTF_KIND_VAR => {
            log.extend(b" type_id=")?;
            log.decimal(meta.size_or_type as usize)?;
            log.extend(b" linkage=")?;
            log.decimal(u32_at(bytes, meta.payload).unwrap() as usize)?;
        }
        BTF_KIND_FLOAT => {
            log.extend(b" size=")?;
            log.decimal(meta.size_or_type as usize)?;
        }
        BTF_KIND_DECL_TAG => {
            log.extend(b" type=")?;
            log.decimal(meta.size_or_type as usize)?;
            log.extend(b" component_idx=")?;
            log.extend_signed(u32_at(bytes, meta.payload).unwrap() as i32 as i64)?;
        }
        _ => {}
    }
    if let Some(reason) = reason {
        log.push(b' ')?;
        reason.append(log)?;
    }
    log.push(b'\n')?;
    Ok(())
}
fn log_member(
    log: &mut BtfDiagnostic,
    bytes: &[u8],
    strings: &StringTable<'_>,
    meta: TypeMeta,
    member: usize,
    reason: Option<LinuxReason>,
) -> AxResult {
    let at = meta.payload + member * 12;
    let raw = u32_at(bytes, at + 8).unwrap();
    log.extend(b"\t")?;
    log_name(log, strings, u32_at(bytes, at).unwrap())?;
    log.extend(b" type_id=")?;
    log.decimal(u32_at(bytes, at + 4).unwrap() as usize)?;
    if meta.kflag {
        log.extend(b" bitfield_size=")?;
        log.decimal((raw >> 24) as usize)?;
        log.extend(b" bits_offset=")?;
        log.decimal((raw & 0x00ff_ffff) as usize)?;
    } else {
        log.extend(b" bits_offset=")?;
        log.decimal(raw as usize)?;
    }
    if let Some(reason) = reason {
        log.push(b' ')?;
        reason.append(log)?;
    }
    log.push(b'\n')
}
fn log_vsi(
    log: &mut BtfDiagnostic,
    bytes: &[u8],
    meta: TypeMeta,
    entry: usize,
    reason: Option<LinuxReason>,
) -> AxResult {
    let at = meta.payload + entry * 12;
    log.extend(b"\t type_id=")?;
    log.decimal(u32_at(bytes, at).unwrap() as usize)?;
    log.extend(b" offset=")?;
    log.decimal(u32_at(bytes, at + 4).unwrap() as usize)?;
    log.extend(b" size=")?;
    log.decimal(u32_at(bytes, at + 8).unwrap() as usize)?;
    if let Some(reason) = reason {
        log.push(b' ')?;
        reason.append(log)?;
    }
    log.push(b'\n')
}
fn log_enum_value(
    log: &mut BtfDiagnostic,
    strings: &StringTable<'_>,
    bytes: &[u8],
    meta: TypeMeta,
    value: usize,
    reason: Option<LinuxReason>,
) -> AxResult {
    let stride = if meta.kind == BTF_KIND_ENUM64 { 12 } else { 8 };
    let at = meta.payload + value * stride;
    log.extend(b"\t")?;
    log_name(log, strings, u32_at(bytes, at).unwrap())?;
    log.extend(b" val=")?;
    let raw = if meta.kind == BTF_KIND_ENUM64 {
        (u32_at(bytes, at + 4).unwrap() as u64) | ((u32_at(bytes, at + 8).unwrap() as u64) << 32)
    } else {
        u32_at(bytes, at + 4).unwrap() as u64
    };
    if meta.kflag {
        log.extend_signed(raw as i64)?;
    } else {
        log.decimal(raw as usize)?;
    }
    if let Some(reason) = reason {
        log.push(b' ')?;
        reason.append(log)?;
    }
    log.push(b'\n')
}

/// Emit exactly the prefix the kernel had accepted, followed by the failed
/// subrecord.  This is also used for a later resolver failure, whose context
/// is rewritten at the source edge rather than printed as a misleading
/// successful target type.
#[derive(Clone, Copy)]
enum FailurePhase {
    Meta,
    Resolve,
}

fn log_failure(
    log: &mut BtfDiagnostic,
    bytes: &[u8],
    strings: &StringTable<'_>,
    types: &[TypeMeta],
    current: Option<TypeMeta>,
    error: SemanticError,
    phase: FailurePhase,
) -> AxResult {
    let (type_id, detail) = match error.context {
        SemanticContext::Type(id) => (id, None),
        SemanticContext::Member { type_id, member } => (type_id, Some((0, member))),
        SemanticContext::Vsi { type_id, entry } => (type_id, Some((1, entry))),
        SemanticContext::EnumValue { type_id, value } => (type_id, Some((3, value))),
    };
    // All semantic failures are tied to a parsed type.  Do not manufacture a
    // generic trailing diagnostic if an internal caller loses that context.
    let Some(meta) = types
        .get(type_id.saturating_sub(1))
        .copied()
        .or_else(|| (type_id == types.len() + 1).then_some(current).flatten())
    else {
        return Err(AxError::InvalidInput);
    };
    if matches!(error.reason, LinuxReason::InvalidInfo(_)) {
        // `btf_parse_type()` has not accepted this type yet: Linux's basic
        // formatter prints only the numeric type id and raw btf_info.
        log.push(b'[')?;
        log.decimal(type_id)?;
        log.extend(b"] ")?;
        error.reason.append(log)?;
        log.push(b'\n')?;
    } else {
        // Resolve-time member/VSI diagnostics deliberately repeat the owning
        // type, but never replay the subrecords already emitted by CHECK_META.
        log_type(
            log,
            bytes,
            strings,
            type_id,
            meta,
            detail.is_none().then_some(error.reason),
        )?;
    }
    let (kind, failed) = match detail {
        Some(value) => value,
        None => return Ok(()),
    };
    if matches!(phase, FailurePhase::Meta) {
        for item in 0..failed {
            match kind {
                0 => log_member(log, bytes, strings, meta, item, None)?,
                1 => log_vsi(log, bytes, meta, item, None)?,
                3 => log_enum_value(log, strings, bytes, meta, item, None)?,
                _ => unreachable!(),
            }
        }
    }
    match kind {
        0 => log_member(log, bytes, strings, meta, failed, Some(error.reason)),
        1 => log_vsi(log, bytes, meta, failed, Some(error.reason)),
        3 => log_enum_value(log, strings, bytes, meta, failed, Some(error.reason)),
        _ => unreachable!(),
    }
}

fn contextualize(meta: TypeMeta, type_id: usize, error: SemanticError) -> SemanticError {
    let context = match meta.kind {
        BTF_KIND_STRUCT | BTF_KIND_UNION
            if error.offset >= meta.payload && error.offset < meta.payload + meta.payload_len =>
        {
            SemanticContext::Member {
                type_id,
                member: (error.offset - meta.payload) / 12,
            }
        }
        BTF_KIND_DATASEC
            if error.offset >= meta.payload && error.offset < meta.payload + meta.payload_len =>
        {
            SemanticContext::Vsi {
                type_id,
                entry: (error.offset - meta.payload) / 12,
            }
        }
        BTF_KIND_ENUM
            if error.offset >= meta.payload && error.offset < meta.payload + meta.payload_len =>
        {
            SemanticContext::EnumValue {
                type_id,
                value: (error.offset - meta.payload) / 8,
            }
        }
        BTF_KIND_ENUM64
            if error.offset >= meta.payload && error.offset < meta.payload + meta.payload_len =>
        {
            SemanticContext::EnumValue {
                type_id,
                value: (error.offset - meta.payload) / 12,
            }
        }
        _ => SemanticContext::Type(type_id),
    };
    error.with_context(context)
}

fn log_success_type(
    log: &mut BtfDiagnostic,
    bytes: &[u8],
    strings: &StringTable<'_>,
    type_id: usize,
    meta: TypeMeta,
) -> AxResult {
    log_type(log, bytes, strings, type_id, meta, None)?;
    match meta.kind {
        BTF_KIND_STRUCT | BTF_KIND_UNION => {
            for member in 0..meta.vlen {
                log_member(log, bytes, strings, meta, member, None)?;
            }
        }
        BTF_KIND_DATASEC => {
            for entry in 0..meta.vlen {
                log_vsi(log, bytes, meta, entry, None)?;
            }
        }
        BTF_KIND_ENUM | BTF_KIND_ENUM64 => {
            for value in 0..meta.vlen {
                log_enum_value(log, strings, bytes, meta, value, None)?;
            }
        }
        _ => {}
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolveState {
    Fresh,
    Active,
    Done,
}
struct ResolveCtx<'a> {
    bytes: &'a [u8],
    types: &'a [TypeMeta],
    state: Vec<ResolveState>,
    resolved_id: Vec<u32>,
    resolved_size: Vec<u64>,
    depth: usize,
}
impl<'a> ResolveCtx<'a> {
    fn new(bytes: &'a [u8], types: &'a [TypeMeta]) -> Result<Self, SemanticError> {
        let mut state = Vec::new();
        let mut resolved_id = Vec::new();
        let mut resolved_size = Vec::new();
        state.try_reserve(types.len()).map_err(|_| SemanticError {
            errno: AxError::NoMemory,
            reason: LinuxReason::InternalNoMemory,
            index: 0,
            offset: 0,
            context: SemanticContext::Type(0),
        })?;
        resolved_id
            .try_reserve(types.len())
            .map_err(|_| SemanticError {
                errno: AxError::NoMemory,
                reason: LinuxReason::InternalNoMemory,
                index: 0,
                offset: 0,
                context: SemanticContext::Type(0),
            })?;
        resolved_size
            .try_reserve(types.len())
            .map_err(|_| SemanticError {
                errno: AxError::NoMemory,
                reason: LinuxReason::InternalNoMemory,
                index: 0,
                offset: 0,
                context: SemanticContext::Type(0),
            })?;
        for _ in types {
            state.push(ResolveState::Fresh);
            resolved_id.push(0);
            resolved_size.push(0);
        }
        Ok(Self {
            bytes,
            types,
            state,
            resolved_id,
            resolved_size,
            depth: 0,
        })
    }
    // Linux calls only VAR, DECL_TAG and DATASEC resolve sources: the other
    // no-size kinds may legitimately be reached through a pointer.
    fn source_only(kind: u32) -> bool {
        matches!(kind, BTF_KIND_VAR | BTF_KIND_DATASEC | BTF_KIND_DECL_TAG)
    }
    fn check_pointer_chain(
        &self,
        start: u32,
        index: usize,
        offset: usize,
    ) -> Result<(), SemanticError> {
        let mut slow = start;
        let mut fast = start;
        let next = |id: u32| -> Option<u32> {
            let ty = type_at(self.types, id)?;
            matches!(
                ty.kind,
                BTF_KIND_PTR
                    | BTF_KIND_TYPEDEF
                    | BTF_KIND_VOLATILE
                    | BTF_KIND_CONST
                    | BTF_KIND_RESTRICT
                    | BTF_KIND_TYPE_TAG
            )
            .then_some(ty.size_or_type)
        };
        for _ in 0..=self.types.len() {
            slow = next(slow).unwrap_or(0);
            fast = next(fast).and_then(|id| next(id)).unwrap_or(0);
            if slow == 0 || fast == 0 {
                return Ok(());
            }
            if slow == fast {
                return Err(SemanticError::invalid(
                    LinuxReason::TypeCycle,
                    index,
                    offset,
                ));
            }
        }
        Err(SemanticError::invalid(
            LinuxReason::TypeDepth,
            index,
            offset,
        ))
    }
    fn resolve_data(
        &mut self,
        id: u32,
        owner: usize,
        offset: usize,
    ) -> Result<(u32, u64), SemanticError> {
        if id == 0 {
            return Err(SemanticError::invalid(
                LinuxReason::InvalidTypeId,
                owner,
                offset,
            ));
        }
        let value = self.resolve(id)?;
        if value.0 == 0 {
            return Err(SemanticError::invalid(
                LinuxReason::InvalidTypeId,
                owner,
                offset,
            ));
        }
        if Self::source_only(self.types[value.0 as usize - 1].kind) {
            return Err(SemanticError::invalid(
                LinuxReason::SourceOnly,
                owner,
                offset,
            ));
        }
        Ok(value)
    }
    fn resolve(&mut self, id: u32) -> Result<(u32, u64), SemanticError> {
        if id == 0 {
            return Ok((0, 0));
        }
        let slot = id as usize - 1;
        if slot >= self.types.len() {
            return Err(SemanticError::invalid(LinuxReason::InvalidTypeId, 0, 0));
        }
        match self.state[slot] {
            ResolveState::Done => return Ok((self.resolved_id[slot], self.resolved_size[slot])),
            ResolveState::Active => {
                return Err(SemanticError::invalid(
                    LinuxReason::TypeCycle,
                    slot + 1,
                    self.types[slot].record,
                ));
            }
            ResolveState::Fresh => {}
        }
        if self.depth >= 32 {
            return Err(SemanticError::invalid(
                LinuxReason::TypeDepth,
                slot + 1,
                self.types[slot].record,
            ));
        }
        self.state[slot] = ResolveState::Active;
        self.depth += 1;
        let meta = self.types[slot];
        let index = slot + 1;
        let result = match meta.kind {
            BTF_KIND_INT | BTF_KIND_ENUM | BTF_KIND_ENUM64 | BTF_KIND_FLOAT => {
                Ok((id, meta.size_or_type as u64))
            }
            BTF_KIND_STRUCT | BTF_KIND_UNION => {
                // Traverse by-value members while this node is active.  A
                // self-reference through PTR stays valid because PTR never
                // follows its pointee; every other recursive aggregate path
                // is a non-representable object layout.
                for member in 0..meta.vlen {
                    let at = meta.payload + member * 12;
                    let _ = self
                        .resolve_data(u32_at(self.bytes, at + 4).unwrap(), index, at + 4)
                        .map_err(|error| {
                            error.with_context(SemanticContext::Member {
                                type_id: index,
                                member,
                            })
                        })?;
                }
                Ok((id, meta.size_or_type as u64))
            }
            BTF_KIND_PTR => {
                // PTR is a size barrier for aggregates, but a chain composed
                // only of PTR/modifiers must still be walked to reject the
                // non-representable pointer loops accepted by a shallow walk.
                if meta.size_or_type != 0 {
                    self.check_pointer_chain(id, index, meta.record + 8)?;
                    let mut target = meta.size_or_type;
                    for _ in 0..=self.types.len() {
                        let ty = type_at(self.types, target).ok_or_else(|| {
                            SemanticError::invalid(
                                LinuxReason::InvalidPointer,
                                index,
                                meta.record + 8,
                            )
                        })?;
                        if Self::source_only(ty.kind) {
                            return Err(SemanticError::invalid(
                                LinuxReason::SourceOnly,
                                index,
                                meta.record + 8,
                            ));
                        }
                        if !matches!(
                            ty.kind,
                            BTF_KIND_PTR
                                | BTF_KIND_TYPEDEF
                                | BTF_KIND_VOLATILE
                                | BTF_KIND_CONST
                                | BTF_KIND_RESTRICT
                                | BTF_KIND_TYPE_TAG
                        ) {
                            break;
                        }
                        if ty.size_or_type == 0 {
                            break;
                        }
                        target = ty.size_or_type;
                    }
                }
                Ok((id, 8))
            }
            BTF_KIND_TYPEDEF | BTF_KIND_VOLATILE | BTF_KIND_CONST | BTF_KIND_RESTRICT
            | BTF_KIND_TYPE_TAG => {
                if meta.size_or_type == 0 {
                    Ok((0, 0))
                } else {
                    let resolved = self.resolve(meta.size_or_type)?;
                    if resolved.0 != 0
                        && Self::source_only(self.types[resolved.0 as usize - 1].kind)
                    {
                        Err(SemanticError::invalid(
                            LinuxReason::SourceOnly,
                            index,
                            meta.record + 8,
                        ))
                    } else {
                        Ok(resolved)
                    }
                }
            }
            BTF_KIND_ARRAY => {
                let element = u32_at(self.bytes, meta.payload).unwrap();
                let count = u32_at(self.bytes, meta.payload + 8).unwrap() as u64;
                let (concrete, size) = self.resolve_data(element, index, meta.payload)?;
                let total = size.checked_mul(count).ok_or_else(|| {
                    SemanticError::invalid(LinuxReason::SizeOverflow, index, meta.payload + 8)
                })?;
                Ok((concrete, total))
            }
            BTF_KIND_FWD | BTF_KIND_FUNC | BTF_KIND_FUNC_PROTO | BTF_KIND_VAR
            | BTF_KIND_DATASEC | BTF_KIND_DECL_TAG => Ok((id, 0)),
            _ => Err(SemanticError::invalid(
                LinuxReason::InvalidKind,
                index,
                meta.record + 4,
            )),
        };
        self.depth -= 1;
        match result {
            Ok((concrete, size)) => {
                self.state[slot] = ResolveState::Done;
                self.resolved_id[slot] = concrete;
                self.resolved_size[slot] = size;
                Ok((concrete, size))
            }
            Err(error) => {
                self.state[slot] = ResolveState::Fresh;
                Err(error)
            }
        }
    }
}

enum ValidationError {
    Parse(BtfParseError),
    Semantic(SemanticError),
}
impl From<SemanticError> for ValidationError {
    fn from(error: SemanticError) -> Self {
        Self::Semantic(error)
    }
}

fn validate_semantics(
    bytes: &[u8],
    sections: BtfSections,
    _log: &mut BtfDiagnostic,
) -> Result<u32, ValidationError> {
    let mut types = Vec::new();
    let mut cursor = sections.type_start;
    let strings = StringTable::from_btf(bytes);
    while cursor < sections.type_end {
        let index = types.len() + 1;
        if index > BTF_MAX_TYPE as usize {
            return Err(ValidationError::Parse(BtfParseError::TooManyTypes {
                offset: cursor,
            }));
        }
        let (meta, next) = parse_one_type(bytes, cursor, sections.type_end, index)
            .map_err(ValidationError::Parse)?;
        // Do not append to the resolver ledger until this record's local
        // metadata checks have succeeded.  `log_failure` receives `meta`
        // separately when it needs to render the rejected record.
        types.try_reserve(1).map_err(|_| {
            ValidationError::Semantic(SemanticError {
                errno: AxError::NoMemory,
                reason: LinuxReason::InternalNoMemory,
                index,
                offset: cursor,
                context: SemanticContext::Type(index),
            })
        })?;
        let fail = |reason, offset| SemanticError::invalid(reason, index, offset);
        let ref_ok = |id| id <= BTF_MAX_TYPE;
        if meta.name != 0 && strings.string(meta.name).is_none() {
            _log.invalid_name_offset(index, meta.name)
                .map_err(|errno| SemanticError {
                    errno,
                    reason: LinuxReason::InternalNoMemory,
                    index,
                    offset: meta.record,
                    context: SemanticContext::Type(index),
                })?;
            return Err(fail(LinuxReason::InvalidNameOffset, meta.record).into());
        }
        // Bits not described by btf_type.info are reserved in v6.18 and must
        // not be silently treated as future flags.
        let info = u32_at(bytes, meta.record + 4).unwrap();
        if info & 0x60ff_0000 != 0 {
            let error = fail(LinuxReason::InvalidInfo(info), meta.record + 4);
            log_failure(
                _log,
                bytes,
                &strings,
                &types,
                Some(meta),
                error,
                FailurePhase::Meta,
            )
            .map_err(|errno| SemanticError {
                errno,
                reason: LinuxReason::InternalNoMemory,
                index,
                offset: meta.record,
                context: SemanticContext::Type(index),
            })?;
            return Err(error.into());
        }
        let meta_result = (|| -> Result<(), SemanticError> {
            match meta.kind {
                BTF_KIND_INT => {
                    let int_data = u32_at(bytes, meta.payload).unwrap();
                    if meta.vlen != 0 {
                        return Err(fail(LinuxReason::IntVlenNonZero, meta.record));
                    }
                    if meta.kflag {
                        return Err(fail(LinuxReason::InvalidBtfInfoKindFlag, meta.record));
                    }
                    if int_data >> 28 != 0 {
                        return Err(fail(LinuxReason::InvalidIntData(int_data), meta.payload));
                    }
                    let encoding = (int_data >> 24) & 0x0f;
                    if !matches!(encoding, 0 | 1 | 2 | 4) {
                        return Err(SemanticError::unsupported(
                            LinuxReason::UnsupportedEncoding,
                            index,
                            meta.payload,
                        ));
                    }
                    let bits = int_data & 0xff;
                    let bit_offset = (int_data >> 16) & 0xff;
                    let nr_bits = bit_offset
                        .checked_add(bits)
                        .ok_or_else(|| fail(LinuxReason::IntBitsExceed128, meta.payload))?;
                    if nr_bits > 128 {
                        return Err(fail(LinuxReason::IntBitsExceed128, meta.payload));
                    }
                    if (nr_bits + 7) / 8 > meta.size_or_type {
                        return Err(fail(LinuxReason::IntBitsExceedTypeSize, meta.payload));
                    }
                }
                BTF_KIND_PTR | BTF_KIND_TYPEDEF | BTF_KIND_VOLATILE | BTF_KIND_CONST
                | BTF_KIND_RESTRICT | BTF_KIND_TYPE_TAG => {
                    if meta.vlen != 0
                        || (meta.kflag && meta.kind != BTF_KIND_TYPE_TAG)
                        || !ref_ok(meta.size_or_type)
                    {
                        return Err(fail(LinuxReason::InvalidTypeExact, meta.record + 8));
                    }
                    if meta.kind == BTF_KIND_TYPEDEF
                        && (meta.name == 0 || !strings.valid_identifier(meta.name))
                    {
                        return Err(fail(LinuxReason::InvalidTypeExact, meta.record));
                    }
                    if meta.kind == BTF_KIND_TYPE_TAG && !strings.nonempty(meta.name) {
                        return Err(fail(LinuxReason::InvalidTypeExact, meta.record));
                    }
                    if matches!(
                        meta.kind,
                        BTF_KIND_PTR | BTF_KIND_VOLATILE | BTF_KIND_CONST | BTF_KIND_RESTRICT
                    ) && meta.name != 0
                    {
                        return Err(fail(LinuxReason::InvalidTypeExact, meta.record));
                    }
                    // A TYPE_TAG is the first qualifier in its BTF chain.  A
                    // later CONST/VOLATILE/TYPEDEF may not point back across it;
                    // accepting CONST -> TYPE_TAG creates an order Linux rejects.
                    if meta.kind != BTF_KIND_TYPE_TAG
                        && type_at(&types, meta.size_or_type)
                            .is_some_and(|target| target.kind == BTF_KIND_TYPE_TAG)
                    {
                        return Err(fail(LinuxReason::InvalidTypeExact, meta.record + 8));
                    }
                }
                BTF_KIND_ARRAY => {
                    if meta.name != 0 {
                        return Err(fail(LinuxReason::InvalidName, meta.record));
                    }
                    if meta.vlen != 0 {
                        return Err(fail(LinuxReason::IntVlenNonZero, meta.record));
                    }
                    if meta.kflag {
                        return Err(fail(LinuxReason::InvalidBtfInfoKindFlag, meta.record));
                    }
                    if meta.size_or_type != 0 {
                        return Err(fail(LinuxReason::InvalidArraySize, meta.record + 8));
                    }
                    // These are metadata constraints rather than graph
                    // resolution: array element and index IDs cannot denote
                    // void and must fit BTF's encoded-ID range.  Linux checks
                    // them in this order before it considers either target.
                    let element = u32_at(bytes, meta.payload).unwrap();
                    if element == 0 || !ref_ok(element) {
                        return Err(fail(LinuxReason::InvalidElem, meta.payload));
                    }
                    let index_type = u32_at(bytes, meta.payload + 4).unwrap();
                    if index_type == 0 || !ref_ok(index_type) {
                        return Err(fail(LinuxReason::InvalidIndex, meta.payload + 4));
                    }
                }
                BTF_KIND_STRUCT | BTF_KIND_UNION => {
                    if meta.name != 0 && !strings.valid_identifier(meta.name) {
                        return Err(fail(LinuxReason::InvalidName, meta.record));
                    }
                    let mut prior = 0u32;
                    for member in 0..meta.vlen {
                        let at = meta.payload + member * 12;
                        let member_name = u32_at(bytes, at).unwrap();
                        let ty = u32_at(bytes, at + 4).unwrap();
                        let bits = u32_at(bytes, at + 8).unwrap();
                        if member_name != 0 && strings.string(member_name).is_none() {
                            return Err(fail(
                                LinuxReason::InvalidMemberNameOffset(member_name),
                                at,
                            ));
                        }
                        if member_name != 0 && !strings.valid_identifier(member_name) {
                            return Err(fail(LinuxReason::InvalidName, at));
                        }
                        if ty == 0 || !ref_ok(ty) {
                            return Err(fail(LinuxReason::InvalidTypeId, at + 4));
                        }
                        let offset = if meta.kflag { bits & 0x00ff_ffff } else { bits };
                        if (meta.kind == BTF_KIND_STRUCT && member != 0 && offset < prior)
                            || (meta.kind == BTF_KIND_UNION && offset != 0)
                        {
                            return Err(fail(LinuxReason::InvalidMemberBitsOffset, at + 8));
                        }
                        prior = offset;
                        if meta.kflag && bits >> 24 != 0 {
                            let width = bits >> 24;
                            if offset
                                .checked_add(width)
                                .map_or(true, |end| end > meta.size_or_type.saturating_mul(8))
                            {
                                return Err(fail(
                                    LinuxReason::MemberBitsOffsetExceedsStructSize,
                                    at + 8,
                                ));
                            }
                        } else if offset % 8 != 0 || offset > meta.size_or_type.saturating_mul(8) {
                            return Err(fail(
                                LinuxReason::MemberBitsOffsetExceedsStructSize,
                                at + 8,
                            ));
                        }
                    }
                }
                BTF_KIND_ENUM => {
                    if meta.name != 0 && !strings.valid_identifier(meta.name) {
                        return Err(fail(LinuxReason::InvalidEnum, meta.record));
                    }
                    if !matches!(meta.size_or_type, 1 | 2 | 4 | 8) {
                        return Err(fail(LinuxReason::InvalidEnum, meta.record));
                    }
                    for value in 0..meta.vlen {
                        if !strings
                            .valid_identifier(u32_at(bytes, meta.payload + value * 8).unwrap())
                        {
                            return Err(fail(LinuxReason::InvalidEnum, meta.payload + value * 8));
                        }
                    }
                }
                // kind_flag carries the signedness of ENUM/ENUM64 and is not a
                // reserved bit.  The values remain raw two's-complement bits.
                BTF_KIND_ENUM64 => {
                    if meta.name != 0 && !strings.valid_identifier(meta.name) {
                        return Err(fail(LinuxReason::InvalidEnum, meta.record));
                    }
                    if !matches!(meta.size_or_type, 1 | 2 | 4 | 8) {
                        return Err(fail(LinuxReason::InvalidEnum, meta.record));
                    }
                    for value in 0..meta.vlen {
                        if !strings
                            .valid_identifier(u32_at(bytes, meta.payload + value * 12).unwrap())
                        {
                            return Err(fail(LinuxReason::InvalidEnum, meta.payload + value * 12));
                        }
                    }
                }
                BTF_KIND_FWD => {
                    if meta.vlen != 0 {
                        return Err(fail(LinuxReason::IntVlenNonZero, meta.record));
                    }
                    if meta.size_or_type != 0 {
                        return Err(fail(LinuxReason::TypeNonZero, meta.record + 8));
                    }
                    if meta.name == 0 || !strings.valid_identifier(meta.name) {
                        return Err(fail(LinuxReason::InvalidName, meta.record));
                    }
                }
                BTF_KIND_FUNC => {
                    if meta.name == 0 || !strings.valid_identifier(meta.name) {
                        return Err(fail(LinuxReason::InvalidName, meta.record));
                    }
                    if meta.vlen > 2 {
                        return Err(fail(LinuxReason::InvalidFuncLinkage, meta.record));
                    }
                    if meta.kflag {
                        return Err(fail(LinuxReason::InvalidBtfInfoKindFlag, meta.record));
                    }
                }
                BTF_KIND_FUNC_PROTO => {
                    if meta.name != 0 {
                        return Err(fail(LinuxReason::InvalidName, meta.record));
                    }
                    if meta.kflag {
                        return Err(fail(LinuxReason::InvalidBtfInfoKindFlag, meta.record));
                    }
                }
                BTF_KIND_VAR => {
                    if meta.vlen != 0 {
                        return Err(fail(LinuxReason::IntVlenNonZero, meta.record));
                    }
                    if meta.kflag {
                        return Err(fail(LinuxReason::InvalidBtfInfoKindFlag, meta.record));
                    }
                    if meta.name == 0 || !strings.valid_identifier(meta.name) {
                        return Err(fail(LinuxReason::InvalidName, meta.record));
                    }
                    if meta.size_or_type == 0 || !ref_ok(meta.size_or_type) {
                        return Err(fail(LinuxReason::InvalidTypeId, meta.record + 8));
                    }
                    if u32_at(bytes, meta.payload).unwrap() > 1 {
                        return Err(fail(LinuxReason::LinkageNotSupported, meta.payload));
                    }
                }
                BTF_KIND_DATASEC => {
                    if meta.size_or_type == 0 {
                        return Err(fail(LinuxReason::SizeIsZero, meta.record));
                    }
                    if meta.kflag {
                        return Err(fail(LinuxReason::InvalidBtfInfoKindFlag, meta.record));
                    }
                    if !strings.valid_section(meta.name) {
                        return Err(fail(LinuxReason::InvalidName, strings.offset(meta.name)));
                    }
                    let mut prior_end = 0;
                    let mut sum = 0u64;
                    for entry in 0..meta.vlen {
                        let at = meta.payload + entry * 12;
                        let offset = u32_at(bytes, at + 4).unwrap();
                        let size = u32_at(bytes, at + 8).unwrap();
                        // A DATASEC may name a VAR which appears later in
                        // the stream; validate its kind in the resolver.
                        if u32_at(bytes, at).unwrap() == 0 {
                            return Err(fail(LinuxReason::InvalidVsiType, at));
                        }
                        if entry != 0 && offset < prior_end || offset >= meta.size_or_type {
                            return Err(fail(LinuxReason::InvalidVsiOffset, at + 4));
                        }
                        if size == 0 || size > meta.size_or_type {
                            return Err(fail(LinuxReason::InvalidVsiSize, at + 8));
                        }
                        let end = offset
                            .checked_add(size)
                            .ok_or_else(|| fail(LinuxReason::InvalidVsiOffsetPlusSize, at + 4))?;
                        if end > meta.size_or_type {
                            return Err(fail(LinuxReason::InvalidVsiOffsetPlusSize, at + 4));
                        }
                        prior_end = end;
                        sum = sum.checked_add(size as u64).ok_or_else(|| {
                            fail(LinuxReason::InvalidVsiInfoSize, meta.record + 8)
                        })?;
                    }
                    if sum > meta.size_or_type as u64 {
                        return Err(fail(LinuxReason::InvalidVsiInfoSize, meta.record + 8));
                    }
                }
                BTF_KIND_FLOAT => {
                    if meta.vlen != 0
                        || meta.kflag
                        || !matches!(meta.size_or_type, 2 | 4 | 8 | 12 | 16)
                    {
                        return Err(fail(LinuxReason::InvalidTypeExact, meta.record));
                    }
                }
                BTF_KIND_DECL_TAG => {
                    let component = u32_at(bytes, meta.payload).unwrap() as i32;
                    // kind_flag is ignored for DECL_TAG by Linux.  Keep the
                    // remaining metadata checks in verifier order so the
                    // reason attached to a malformed record is stable.
                    if meta.name == 0 || !strings.nonempty(meta.name) {
                        return Err(fail(LinuxReason::InvalidValue, meta.record));
                    }
                    if meta.vlen != 0 {
                        return Err(fail(LinuxReason::IntVlenNonZero, meta.record));
                    }
                    if component < -1 {
                        return Err(fail(LinuxReason::InvalidComponentIdx, meta.payload));
                    }
                }
                _ => return Err(fail(LinuxReason::InvalidKind, meta.record + 4)),
            };
            Ok(())
        })();
        if let Err(error) = meta_result {
            let error = contextualize(meta, index, error);
            log_failure(
                _log,
                bytes,
                &strings,
                &types,
                Some(meta),
                error,
                FailurePhase::Meta,
            )
            .map_err(|errno| SemanticError {
                errno,
                reason: LinuxReason::InternalNoMemory,
                index,
                offset: meta.record,
                context: SemanticContext::Type(index),
            })?;
            return Err(error.into());
        }
        log_success_type(_log, bytes, &strings, index, meta).map_err(|errno| SemanticError {
            errno,
            reason: LinuxReason::InternalNoMemory,
            index,
            offset: meta.record,
            context: SemanticContext::Type(index),
        })?;
        types.push(meta);
        cursor = next;
    }
    let mut resolve = ResolveCtx::new(bytes, &types)?;
    let resolution = (|| -> Result<(), SemanticError> {
        // Resolve every relationship which needs a concrete object type after all
        // headers have been checked.  This makes cross-kind cycles and overflow
        // fail at the referring record, rather than leaking to object consumers.
        for (slot, meta) in types.iter().enumerate() {
            let index = slot + 1;
            if matches!(
                meta.kind,
                BTF_KIND_PTR
                    | BTF_KIND_TYPEDEF
                    | BTF_KIND_VOLATILE
                    | BTF_KIND_CONST
                    | BTF_KIND_RESTRICT
                    | BTF_KIND_TYPE_TAG
            ) && meta.size_or_type != 0
                && type_at(&types, meta.size_or_type).is_none()
            {
                return Err(SemanticError::invalid(
                    LinuxReason::InvalidTypeId,
                    index,
                    meta.record + 8,
                ));
            }
            if meta.kind != BTF_KIND_TYPE_TAG
                && matches!(
                    meta.kind,
                    BTF_KIND_PTR
                        | BTF_KIND_TYPEDEF
                        | BTF_KIND_VOLATILE
                        | BTF_KIND_CONST
                        | BTF_KIND_RESTRICT
                )
                && type_at(&types, meta.size_or_type)
                    .is_some_and(|target| target.kind == BTF_KIND_TYPE_TAG)
            {
                return Err(SemanticError::invalid(
                    LinuxReason::InvalidTypeExact,
                    index,
                    meta.record + 8,
                ));
            }
            // Sweep from the owning record only after its direct edges have
            // been checked.  This prevents an out-of-range forward ID from
            // escaping as synthetic Type(0) before its source can format the
            // verifier diagnostic.
            if meta.kind != BTF_KIND_ARRAY {
                let _ = resolve.resolve(index as u32).map_err(|error| {
                    if error.index == 0 {
                        SemanticError::invalid(LinuxReason::InvalidTypeId, index, meta.record + 8)
                    } else {
                        error
                    }
                })?;
            }
            match meta.kind {
                BTF_KIND_ARRAY => {
                    let element = u32_at(bytes, meta.payload).unwrap();
                    let index_type = u32_at(bytes, meta.payload + 4).unwrap();
                    // Linux validates the index shape before inspecting the
                    // element, so two malformed IDs report Invalid index.
                    if index_type == 0 || type_at(&types, index_type).is_none() {
                        return Err(SemanticError::invalid(
                            LinuxReason::InvalidIndex,
                            index,
                            meta.payload + 4,
                        ));
                    }
                    let (index_concrete, _) = resolve
                        .resolve_data(index_type, index, meta.payload + 4)
                        .map_err(|_| {
                            SemanticError::invalid(
                                LinuxReason::InvalidIndex,
                                index,
                                meta.payload + 4,
                            )
                        })?;
                    if !type_at(&types, index_concrete).is_some_and(|ty| regular_int(bytes, *ty)) {
                        return Err(SemanticError::invalid(
                            LinuxReason::InvalidIndex,
                            index,
                            meta.payload + 4,
                        ));
                    }
                    if element == 0 || type_at(&types, element).is_none() {
                        return Err(SemanticError::invalid(
                            LinuxReason::InvalidElem,
                            index,
                            meta.payload,
                        ));
                    }
                    let (element_concrete, element_size) = resolve
                        .resolve_data(element, index, meta.payload)
                        .map_err(|_| {
                            SemanticError::invalid(LinuxReason::InvalidElem, index, meta.payload)
                        })?;
                    if type_at(&types, element_concrete)
                        .is_some_and(|ty| ty.kind == BTF_KIND_INT && !regular_int(bytes, *ty))
                    {
                        return Err(SemanticError::invalid(
                            LinuxReason::InvalidArrayOfInt,
                            index,
                            meta.payload,
                        ));
                    }
                    let nr_elems = u32_at(bytes, meta.payload + 8).unwrap() as u64;
                    if element_size
                        .checked_mul(nr_elems)
                        .map_or(true, |size| size > u32::MAX as u64)
                    {
                        return Err(SemanticError::invalid(
                            LinuxReason::ArraySizeOverflowsU32,
                            index,
                            meta.payload + 8,
                        ));
                    }
                }
                BTF_KIND_STRUCT | BTF_KIND_UNION => {
                    for member in 0..meta.vlen {
                        let at = meta.payload + member * 12;
                        let context = SemanticContext::Member {
                            type_id: index,
                            member,
                        };
                        let (concrete, size) = resolve
                            .resolve_data(u32_at(bytes, at + 4).unwrap(), index, at + 4)
                            .map_err(|e| e.with_context(context))?;
                        let member_type = type_at(&types, u32_at(bytes, at + 4).unwrap()).unwrap();
                        if size == 0 && member_type.kind != BTF_KIND_ARRAY {
                            return Err(SemanticError::invalid(
                                LinuxReason::InvalidTypeExact,
                                index,
                                at + 4,
                            )
                            .with_context(context));
                        }
                        let raw = u32_at(bytes, at + 8).unwrap();
                        let offset = if meta.kflag { raw & 0x00ff_ffff } else { raw };
                        let width = if meta.kflag { raw >> 24 } else { 0 };
                        if width != 0
                            && !matches!(
                                type_at(&types, concrete).unwrap().kind,
                                BTF_KIND_INT | BTF_KIND_ENUM | BTF_KIND_ENUM64
                            )
                        {
                            return Err(SemanticError::invalid(
                                LinuxReason::InvalidTypeExact,
                                index,
                                at + 4,
                            )
                            .with_context(context));
                        }
                        let occupied = if width != 0 {
                            width
                        } else {
                            u32::try_from(size.checked_mul(8).ok_or_else(|| {
                                SemanticError::invalid(LinuxReason::SizeOverflow, index, at + 4)
                                    .with_context(context)
                            })?)
                            .map_err(|_| {
                                SemanticError::invalid(LinuxReason::SizeOverflow, index, at + 4)
                                    .with_context(context)
                            })?
                        };
                        if offset
                            .checked_add(occupied)
                            .map_or(true, |end| end > meta.size_or_type.saturating_mul(8))
                        {
                            return Err(SemanticError::invalid(
                                LinuxReason::MemberExceedsSize,
                                index,
                                at + 8,
                            )
                            .with_context(context));
                        }
                    }
                }
                BTF_KIND_FUNC => {
                    if !type_at(&types, meta.size_or_type)
                        .is_some_and(|ty| ty.kind == BTF_KIND_FUNC_PROTO)
                    {
                        return Err(SemanticError::invalid(
                            LinuxReason::InvalidTypeId,
                            index,
                            meta.record + 8,
                        ));
                    }
                }
                BTF_KIND_FUNC_PROTO => {
                    if meta.size_or_type != 0 {
                        let (_, size) = resolve
                            .resolve_data(meta.size_or_type, index, meta.record + 8)
                            .map_err(|_| {
                                SemanticError::invalid(
                                    LinuxReason::InvalidReturnType,
                                    index,
                                    meta.record + 8,
                                )
                            })?;
                        if size == 0 {
                            return Err(SemanticError::invalid(
                                LinuxReason::InvalidReturnType,
                                index,
                                meta.record + 8,
                            ));
                        }
                    }
                    let mut argc = meta.vlen;
                    if argc != 0 && u32_at(bytes, meta.payload + (argc - 1) * 8 + 4).unwrap() == 0 {
                        if u32_at(bytes, meta.payload + (argc - 1) * 8).unwrap() != 0 {
                            return Err(SemanticError::invalid_arg(
                                index,
                                argc,
                                meta.payload + (argc - 1) * 8,
                            ));
                        }
                        argc -= 1;
                    }
                    for parameter in 0..argc {
                        let at = meta.payload + parameter * 8;
                        let id = u32_at(bytes, at + 4).unwrap();
                        let (_, size) = resolve.resolve_data(id, index, at + 4).map_err(|_| {
                            SemanticError::invalid_arg(index, parameter + 1, at + 4)
                        })?;
                        if size == 0
                            || (u32_at(bytes, at).unwrap() != 0
                                && !strings.valid_identifier(u32_at(bytes, at).unwrap()))
                        {
                            return Err(SemanticError::invalid_arg(index, parameter + 1, at + 4));
                        }
                    }
                }
                BTF_KIND_VAR => {
                    let _ = resolve
                        .resolve_data(meta.size_or_type, index, meta.record + 8)
                        .map_err(|e| e.with_context(SemanticContext::Type(index)))?;
                }
                BTF_KIND_DATASEC => {
                    for entry in 0..meta.vlen {
                        let at = meta.payload + entry * 12;
                        let context = SemanticContext::Vsi {
                            type_id: index,
                            entry,
                        };
                        let var = type_at(&types, u32_at(bytes, at).unwrap()).ok_or_else(|| {
                            SemanticError::invalid(LinuxReason::InvalidVsiType, index, at)
                                .with_context(context)
                        })?;
                        if var.kind != BTF_KIND_VAR {
                            return Err(SemanticError::invalid(
                                LinuxReason::InvalidVsiType,
                                index,
                                at,
                            )
                            .with_context(context));
                        }
                        let (_, size) = resolve
                            .resolve_data(var.size_or_type, index, at)
                            .map_err(|e| e.with_context(context))?;
                        if size == 0 || (u32_at(bytes, at + 8).unwrap() as u64) < size {
                            return Err(SemanticError::invalid(
                                LinuxReason::InvalidVsiSize,
                                index,
                                at + 8,
                            )
                            .with_context(context));
                        }
                    }
                }
                // DECL_TAG is metadata on both concrete aggregates and function
                // prototypes/parameters.  Unlike an object-bearing edge it must
                // retain that source-level target rather than force a size.
                BTF_KIND_DECL_TAG => {
                    let component = u32_at(bytes, meta.payload).unwrap() as i32;
                    let target = type_at(&types, meta.size_or_type).ok_or_else(|| {
                        SemanticError::invalid(LinuxReason::InvalidTypeId, index, meta.record + 8)
                    })?;
                    if !matches!(
                        target.kind,
                        BTF_KIND_FUNC | BTF_KIND_STRUCT | BTF_KIND_VAR | BTF_KIND_TYPEDEF
                    ) {
                        return Err(SemanticError::invalid(
                            LinuxReason::InvalidDeclTag,
                            index,
                            meta.record + 8,
                        ));
                    }
                    if component >= 0 {
                        let limit = match target.kind {
                            BTF_KIND_STRUCT => target.vlen,
                            BTF_KIND_FUNC => {
                                type_at(&types, target.size_or_type).map_or(0, |proto| proto.vlen)
                            }
                            _ => {
                                return Err(SemanticError::invalid(
                                    LinuxReason::InvalidComponentIdx,
                                    index,
                                    meta.payload,
                                ));
                            }
                        };
                        if component as usize >= limit {
                            return Err(SemanticError::invalid(
                                LinuxReason::InvalidComponentIdx,
                                index,
                                meta.payload,
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    if let Err(error) = resolution {
        let type_id = match error.context {
            SemanticContext::Type(id)
            | SemanticContext::Member { type_id: id, .. }
            | SemanticContext::Vsi { type_id: id, .. }
            | SemanticContext::EnumValue { type_id: id, .. } => id,
        };
        log_failure(
            _log,
            bytes,
            &strings,
            &types,
            None,
            error,
            FailurePhase::Resolve,
        )
        .map_err(|errno| SemanticError {
            errno,
            reason: LinuxReason::InternalNoMemory,
            index: type_id,
            offset: 0,
            context: SemanticContext::Type(type_id),
        })?;
        return Err(error.into());
    }
    Ok(types.len() as u32)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let tail = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(tail.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIND_FLAG: u32 = 1 << 31;

    fn type_record(name: u32, kind: u32, vlen: u16, kind_flag: bool, size_or_type: u32) -> Vec<u8> {
        let mut record = Vec::new();
        record.extend_from_slice(&name.to_le_bytes());
        let info = (kind << 24) | vlen as u32 | if kind_flag { KIND_FLAG } else { 0 };
        record.extend_from_slice(&info.to_le_bytes());
        record.extend_from_slice(&size_or_type.to_le_bytes());
        record
    }

    fn btf(types: Vec<u8>, strings: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BTF_MAGIC.to_le_bytes());
        bytes.push(BTF_VERSION);
        bytes.push(0);
        bytes.extend_from_slice(&(BTF_HEADER_LEN as u32).to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&(types.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(types.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&types);
        bytes.extend_from_slice(strings);
        bytes
    }

    fn diagnostic_bytes(diagnostic: &BtfDiagnostic) -> Vec<u8> {
        let (first, second) = diagnostic.window_slices();
        let mut bytes = Vec::with_capacity(first.len() + second.len());
        bytes.extend_from_slice(first);
        bytes.extend_from_slice(second);
        bytes
    }

    fn assert_invalid(bytes: Vec<u8>, reason: &[u8]) {
        let (errno, diagnostic) = match parse(bytes, 4096, true) {
            Ok(_) => panic!("malformed BTF accepted"),
            Err(error) => error,
        };
        assert_eq!(errno, AxError::InvalidInput);
        assert!(
            diagnostic_bytes(&diagnostic)
                .windows(reason.len())
                .any(|window| window == reason),
            "diagnostic did not contain {}",
            core::str::from_utf8(reason).unwrap(),
        );
    }

    #[test]
    fn array_meta_reasons_preserve_elem_then_index_priority() {
        let mut record = type_record(0, BTF_KIND_ARRAY, 0, false, 0);
        record.extend_from_slice(&0_u32.to_le_bytes());
        record.extend_from_slice(&0_u32.to_le_bytes());
        record.extend_from_slice(&1_u32.to_le_bytes());
        assert_invalid(btf(record, b"\0"), b"Invalid elem");

        let mut record = type_record(0, BTF_KIND_ARRAY, 0, false, 0);
        // A self-reference is a valid encoded ID at this metadata stage; the
        // index check must therefore win before graph resolution.
        record.extend_from_slice(&1_u32.to_le_bytes());
        record.extend_from_slice(&0_u32.to_le_bytes());
        record.extend_from_slice(&1_u32.to_le_bytes());
        assert_invalid(btf(record, b"\0"), b"Invalid index");
    }

    #[test]
    fn fwd_meta_reasons_preserve_vlen_type_name_priority() {
        let record = type_record(0, BTF_KIND_FWD, 1, false, 1);
        assert_invalid(btf(record, b"\0"), b"vlen != 0");

        let record = type_record(0, BTF_KIND_FWD, 0, false, 1);
        assert_invalid(btf(record, b"\0"), b"type != 0");

        let record = type_record(0, BTF_KIND_FWD, 0, false, 0);
        assert_invalid(btf(record, b"\0"), b"Invalid name");
    }

    #[test]
    fn decl_tag_accepts_kind_flag_and_reports_value_then_component() {
        let strings = b"\0s\0tag\0";
        let mut types = type_record(1, BTF_KIND_STRUCT, 0, false, 0);
        let mut tag = type_record(3, BTF_KIND_DECL_TAG, 0, true, 1);
        tag.extend_from_slice(&(-1_i32).to_le_bytes());
        types.extend_from_slice(&tag);
        assert!(parse(btf(types, strings), 4096, true).is_ok());

        let mut tag = type_record(0, BTF_KIND_DECL_TAG, 1, true, 1);
        tag.extend_from_slice(&(-2_i32).to_le_bytes());
        assert_invalid(btf(tag, b"\0"), b"Invalid value");

        let mut tag = type_record(3, BTF_KIND_DECL_TAG, 0, true, 1);
        tag.extend_from_slice(&(-2_i32).to_le_bytes());
        assert_invalid(btf(tag, strings), b"Invalid component_idx");
    }
}

/// Model `btf_parse_hdr()`'s copy boundary.  The only raw read is the
/// minimum field needed to determine `hdr_len`; all later header validation
/// operates on this zero-extended copy.
fn copy_btf_header(bytes: &[u8]) -> Result<([u8; BTF_HEADER_LEN], usize), BtfParseError> {
    let hdr_len = u32_at(bytes, 4).ok_or(BtfParseError::Header {
        message: b"missing header length",
        offset: 4,
    })? as usize;
    if bytes.len() < hdr_len {
        return Err(BtfParseError::Header {
            message: b"header length outside input",
            offset: 4,
        });
    }
    let mut header = [0u8; BTF_HEADER_LEN];
    let copied = core::cmp::min(hdr_len, BTF_HEADER_LEN);
    header[..copied].copy_from_slice(&bytes[..copied]);
    Ok((header, hdr_len))
}

/// Validate every variable-size type record before retaining it.  This is
/// deliberately stricter than just checking section bounds: a malformed BTF
/// graph must never become a long-lived object exposed by ID lookup.
fn validate(bytes: &[u8]) -> Result<BtfSections, BtfParseError> {
    let (header, hdr_len) = copy_btf_header(bytes)?;
    if u16::from_le_bytes(header[0..2].try_into().unwrap()) != BTF_MAGIC {
        return Err(BtfParseError::Header {
            message: b"invalid BTF magic",
            offset: 0,
        });
    }
    if header[2] != BTF_VERSION {
        return Err(BtfParseError::UnsupportedFeature {
            message: b"unsupported BTF version",
            offset: 2,
        });
    }
    if header[3] != 0 {
        return Err(BtfParseError::UnsupportedFeature {
            message: b"unsupported BTF header flags",
            offset: 3,
        });
    }
    if hdr_len > BTF_HEADER_LEN && bytes[BTF_HEADER_LEN..hdr_len].iter().any(|byte| *byte != 0) {
        return Err(BtfParseError::UnsupportedHeader {
            offset: BTF_HEADER_LEN,
        });
    }
    let type_off = u32_at(&header, 8).ok_or(BtfParseError::Section {
        message: b"missing type offset",
        offset: 8,
    })? as usize;
    let type_len = u32_at(&header, 12).ok_or(BtfParseError::Section {
        message: b"missing type length",
        offset: 12,
    })? as usize;
    if type_off % 4 != 0 {
        return Err(BtfParseError::UnalignedTypeOffset { offset: 8 });
    }
    let str_off = u32_at(&header, 16).ok_or(BtfParseError::Section {
        message: b"missing string offset",
        offset: 16,
    })? as usize;
    let str_len = u32_at(&header, 20).ok_or(BtfParseError::Section {
        message: b"missing string length",
        offset: 20,
    })? as usize;
    let type_start = hdr_len
        .checked_add(type_off)
        .ok_or(BtfParseError::Section {
            message: b"type offset overflow",
            offset: 8,
        })?;
    let type_end = type_start
        .checked_add(type_len)
        .ok_or(BtfParseError::Section {
            message: b"type length overflow",
            offset: 12,
        })?;
    let str_start = hdr_len.checked_add(str_off).ok_or(BtfParseError::Section {
        message: b"string offset overflow",
        offset: 16,
    })?;
    let str_end = str_start
        .checked_add(str_len)
        .ok_or(BtfParseError::Section {
            message: b"string length overflow",
            offset: 20,
        })?;
    if type_end > bytes.len()
        || str_end > bytes.len()
        || type_start > type_end
        || str_start > str_end
    {
        return Err(BtfParseError::Section {
            message: b"section range outside input",
            offset: hdr_len,
        });
    }
    // v6.18 accepts only the two known sections.  They cover the complete
    // post-header image in order, with strings ending at EOF; gaps, overlap,
    // or a future section hidden between them are rejected before parsing.
    if type_off != 0 || str_off != type_len || type_end != str_start || str_end != bytes.len() {
        return Err(BtfParseError::Section {
            message: b"sections are not contiguous or leave unsupported data",
            offset: hdr_len,
        });
    }
    if type_len == 0 {
        return Err(BtfParseError::NoTypeFound { offset: 12 });
    }
    if str_len == 0 || bytes[str_start] != 0 || bytes[str_end - 1] != 0 {
        return Err(BtfParseError::StringSectionInvalid { offset: str_start });
    }
    Ok(BtfSections {
        type_start,
        type_end,
    })
}
