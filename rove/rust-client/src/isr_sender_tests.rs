use super::*;
use rove_client_core::{schema::{ClassDesc,PropertyDesc},world::WorldStore};
use serde_json::{json,Value};

fn schema(bits: usize, pv_bit: usize) -> Schema {
    Schema {version:3,flags:0,enums:vec![],classes:vec![ClassDesc {id:0,name:"Part".into(),properties:vec![],events:vec![],
        isr_properties:(0..bits as u32).collect(),unknown_u16:0}],
        properties:(0..bits).map(|i| PropertyDesc {id:i,class:0,name:if i==pv_bit {"ReplicationPV".into()} else {format!("unused{i}")},
            type_id:if i==pv_bit {57} else {1},enum_id:None,lr_type:None}).collect(),
        events:vec![],strings:vec![],names:vec![],lr_types:vec![],parent_property_id:0,decoded_bytes:0}
}
fn view() -> WorldView {
    WorldStore::from_snapshot(&json!({"version":1,"nodes":[{"id":"1234:42","class_name":"Part","properties":{}}]})).unwrap().view()
}
fn frame(case: &Value) -> PartFrame {
    PartFrame {id:case["id"].as_u64().unwrap(),
        position:std::array::from_fn(|i|case["position"][i].as_f64().unwrap()),
        rotation:std::array::from_fn(|i|case["rotation"][i].as_f64().unwrap()),
        linear:std::array::from_fn(|i|case["linear"][i].as_f64().unwrap()),
        angular:std::array::from_fn(|i|case["angular"][i].as_f64().unwrap())}
}
fn fixtures() -> Value {serde_json::from_str(include_str!("../tests/fixtures/isr-owned-parts.json")).unwrap()}
fn hex(data: &[u8]) -> String {data.iter().map(|b|format!("{b:02x}")).collect()}

#[test]
fn isr_sender_matches_python_wide_masks_and_received_decoder() {
    for case in fixtures().as_array().unwrap() {
        let schema=schema(case["bits"].as_u64().unwrap() as usize,case["pv_bit"].as_u64().unwrap() as usize);
        let frame=frame(case);
        let record=record(&schema,&view(),&frame).unwrap();
        assert_eq!(hex(&record),case["record_hex"]);
        let payload=packet(case["local_step"].as_u64().unwrap() as u32,&[record]).unwrap();
        assert_eq!(hex(&payload),case["packet_hex"]);
        let decoded=rove_client_core::instance_state::decode_packet(&payload,&schema,true).unwrap();
        assert_eq!(decoded.header_value,i32::MAX); // unknown offset, not a made-up observation
        assert_eq!(decoded.timestamp,case["local_step"].as_u64().unwrap() as u32);
        assert_eq!(decoded.records.len(),1);
        assert_eq!(decoded.records[0].id.as_deref(),Some("1234:42"));
        assert_eq!(decoded.records[0].fields.len(),1);
        let pv=&decoded.records[0].fields["ReplicationPV"];
        for i in 0..3 {assert!((pv["position"][i].as_f64().unwrap()-frame.position[i]).abs()<0.02);}
        assert!(rove_client_core::instance_state::decode_packet(&payload,&schema,false).is_err());
    }
}

#[test]
fn isr_sender_rejects_missing_ambiguous_or_wrong_property_schemas() {
    let frame=frame(&fixtures()[0]);
    for mut schema in [schema(0,0),schema(257,0),schema(2,0),schema(2,0),schema(2,0)] {
        match schema.classes[0].isr_properties.len() {
            2 => {schema.properties[0].type_id=28;},
            _=>{}
        }
        assert!(record(&schema,&view(),&frame).is_err());
    }
    let mut schema=schema(2,0);schema.properties[1].name="ReplicationPV".into();schema.properties[1].type_id=57;
    assert_eq!(record(&schema,&view(),&frame).unwrap_err(),"isr-pv-schema-ambiguous");
    schema.properties[1].name="unused1".into();schema.classes[0].isr_properties[1]=42;
    assert_eq!(record(&schema,&view(),&frame).unwrap_err(),"isr-property-index-invalid");
    schema.classes[0].isr_properties=vec![1];
    assert_eq!(record(&schema,&view(),&frame).unwrap_err(),"isr-pv-schema-unavailable");
}

#[test]
fn isr_sender_batches_are_bounded_and_preserve_all_records() {
    let schema=schema(2,0);let record=record(&schema,&view(),&frame(&fixtures()[0])).unwrap();
    for n in 1..=8 {
        let payload=packet(0,&vec![record.clone();n]).unwrap();
        assert!(payload.len()<=1100);
        assert_eq!(rove_client_core::instance_state::decode_packet(&payload,&schema,true).unwrap().records.len(),n);
    }
    assert!(packet(0,&[]).is_err());assert!(packet(0,&[vec![]]).is_err());
    assert!(packet(0,&vec![record;9]).is_err());assert!(packet(0,&[vec![1;1100]]).is_err());
}

#[test]
fn observed_clock_offset_is_negated_and_reserved_values_are_rejected() {
    let records=vec![vec![1]];
    for offset in [Some(568),Some(-413),Some(0),None] {
        let wire=packet_with_offset(77,&records,offset).unwrap();
        assert_eq!(i32::from_le_bytes(wire[9..13].try_into().unwrap()),offset.map_or(i32::MAX,|v|-v));
    }
    for reserved in [i32::MIN,i32::MIN+1,i32::MAX] {
        assert!(packet_with_offset(77,&records,Some(reserved)).is_err());
    }
}
