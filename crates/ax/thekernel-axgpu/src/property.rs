/// Value domain for a display property.  The definitions are OS-independent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PropertyKind {
    Boolean,
    Unsigned { min: u64, max: u64 },
    Immutable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Property {
    pub id: u16,
    pub name: &'static str,
    pub kind: PropertyKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PropertyValue {
    pub id: u16,
    pub value: u64,
}

impl Property {
    pub const fn accepts(self, value: u64) -> bool {
        match self.kind {
            PropertyKind::Boolean => value <= 1,
            PropertyKind::Unsigned { min, max } => value >= min && value <= max,
            PropertyKind::Immutable => false,
        }
    }
}
