use thekernel_linux_packet::{
    AF_PACKET, BindPublication, DeliveryDecision, DeliveryDirection, FrameLayout, GetPacketOption,
    InterfaceIndex, LinkLayerAddress, LinkLayerInfo, MSG_PEEK, MSG_TRUNC, PacketBindRequest,
    PacketError, PacketOption, PacketOptionOperation, PacketOptionValue, PacketSendAddress,
    PacketSocketState, PacketSocketType, PacketStatistics, ProtocolSelector, QueueDisposition,
    ReceiveFlags, SetPacketOption,
};

#[test]
fn normalized_bind_receive_and_statistics_contract_is_public() {
    let creation_protocol = ProtocolSelector::from_network_order_i32(i32::from(0x0800_u16.to_be()));
    let mut socket = PacketSocketState::new(PacketSocketType::Raw, creation_protocol);

    let request = PacketBindRequest::try_from_network_order_fields(AF_PACKET, 0, 11).unwrap();
    let plan = socket.prepare_bind(request).unwrap();
    assert_eq!(plan.expected().protocol().host_order(), 0x0800);
    assert_eq!(plan.replacement().protocol().host_order(), 0x0800);
    assert_eq!(socket.publish_bind(plan), Ok(BindPublication::Rebound));

    let mac = LinkLayerAddress::new([2, 0, 0, 0, 0, 1, 99, 99], 6).unwrap();
    let link = LinkLayerInfo::new(InterfaceIndex::exact(11).unwrap(), 1, mac).unwrap();
    let name = socket.get_name(Some(link)).unwrap();
    assert_eq!(name.interface().raw(), 11);
    assert_eq!(name.protocol().host_order(), 0x0800);
    assert_eq!(name.protocol_network_order(), 0x0800_u16.to_be());
    assert_eq!(name.address().as_bytes(), &[2, 0, 0, 0, 0, 1]);

    let raw_destination = [2, 0x66, 0x77, 0x88, 0x99, 0xaa, 7, 8];
    let send_address =
        PacketSendAddress::try_from_network_order_fields(0, 11, 9, raw_destination).unwrap();
    assert_eq!(send_address.protocol(), ProtocolSelector::Disabled);
    assert_eq!(send_address.declared_address_len(), 9);
    assert_eq!(
        send_address.address_for_device(6).unwrap().as_bytes(),
        &raw_destination[..6]
    );

    let flags = ReceiveFlags::from_bits(MSG_PEEK | MSG_TRUNC).unwrap();
    let view = FrameLayout::new(128, 14)
        .unwrap()
        .captured_view(PacketSocketType::Raw, 96)
        .unwrap();
    let receive = view.receive_decision(32, flags);
    assert_eq!(receive.copy_len(), 32);
    assert_eq!(receive.returned_len(), 96);
    assert!(receive.message_truncated());
    assert_eq!(receive.queue_disposition(), QueueDisposition::Retain);
    assert!(!receive.queue_disposition().claims_before_copy());

    let ordinary = view.receive_decision(32, ReceiveFlags::EMPTY);
    assert_eq!(ordinary.queue_disposition(), QueueDisposition::Consume);
    assert!(ordinary.queue_disposition().claims_before_copy());

    assert_eq!(
        socket.delivery_decision(
            DeliveryDirection::Incoming,
            0x0800,
            InterfaceIndex::exact(11).unwrap(),
        ),
        DeliveryDecision::Deliver
    );
    assert_eq!(
        socket.delivery_decision(
            DeliveryDirection::Outgoing,
            0x0800,
            InterfaceIndex::exact(11).unwrap(),
        ),
        DeliveryDecision::OutgoingRequiresAllProtocols
    );

    socket.set_option(SetPacketOption::decode(PacketOption::IgnoreOutgoing.raw(), 1).unwrap());
    assert!(socket.ignore_outgoing());
    assert_eq!(
        SetPacketOption::decode(PacketOption::IgnoreOutgoing.raw(), 2),
        Err(PacketError::InvalidPacketOptionValue)
    );

    assert_eq!(
        GetPacketOption::decode(PacketOption::Statistics.raw()),
        Ok(GetPacketOption::Statistics)
    );
    let endpoint_snapshot = PacketStatistics::from_destructive_snapshot(3, 2, 1, 1);
    let stats = match PacketOptionValue::Statistics(endpoint_snapshot) {
        PacketOptionValue::Statistics(stats) => stats,
        PacketOptionValue::IgnoreOutgoing(_) => panic!("statistics value changed variant"),
    };
    assert_eq!(stats.packets(), 3);
    assert_eq!(stats.drops(), 2);
    assert_eq!(stats.filter_rejected(), 1);
    assert_eq!(stats.filter_errors(), 1);
}

#[test]
fn unsupported_surface_is_never_silently_accepted() {
    assert_eq!(
        GetPacketOption::decode(PacketOption::ReceiveRing.raw()),
        Err(PacketError::UnsupportedPacketOption {
            option: PacketOption::ReceiveRing,
            operation: PacketOptionOperation::Get,
        })
    );
    assert_eq!(
        PacketOption::from_raw(10_000),
        Err(PacketError::UnknownPacketOption)
    );
    assert_eq!(
        PacketSocketType::from_raw(1),
        Err(PacketError::UnsupportedSocketType)
    );
}

#[test]
fn public_interface_indexes_have_one_validated_representation() {
    assert_eq!(InterfaceIndex::default(), InterfaceIndex::Any);
    assert!(matches!(InterfaceIndex::default(), InterfaceIndex::Any));
    assert_eq!(format!("{:?}", InterfaceIndex::Any), "Any");
    assert!(InterfaceIndex::Any.is_any());
    assert_eq!(InterfaceIndex::Any.raw(), 0);
    assert_eq!(InterfaceIndex::Any.exact_value(), None);

    let first = InterfaceIndex::from_raw(1).unwrap();
    assert!(!first.is_any());
    assert_eq!(first.raw(), 1);
    assert_eq!(first.exact_value(), Some(1));
    assert_eq!(format!("{first:?}"), "Exact(1)");

    let maximum = InterfaceIndex::exact(i32::MAX as u32).unwrap();
    assert_eq!(maximum.raw(), i32::MAX);
    assert_eq!(maximum.exact_value(), Some(i32::MAX as u32));

    assert_eq!(
        InterfaceIndex::from_raw(-1),
        Err(PacketError::InvalidInterfaceIndex)
    );
    assert_eq!(
        InterfaceIndex::exact(0),
        Err(PacketError::InvalidInterfaceIndex)
    );
    assert_eq!(
        InterfaceIndex::exact(i32::MAX as u32 + 1),
        Err(PacketError::InvalidInterfaceIndex)
    );
}
