//! Instance decoding: every type code, nested structs and arrays, enums,
//! pointers, strings, cycles and limits. All documents are synthetic.

mod common;

use common::{DocBuilder, E, M, S, W, m, pool_ref};
use u_rome_core::joaat;
use u_rome_pso::{
    DecodeOptions, Hash32, HashMapNames, PsoDocument, PsoError, Value, script_dom,
    to_json_with_schema, type_code as tc,
};

fn h(s: &str) -> u32 {
    joaat(s).0
}
fn hv(s: &str) -> Hash32 {
    joaat(s)
}

const KIND: u32 = 0x649e_cf8e;
const ANON: u32 = 0x100;

fn strict() -> DecodeOptions {
    DecodeOptions {
        strict: true,
        ..Default::default()
    }
}

/// A struct with one member of every type code, plus pools it references.
fn all_types() -> DocBuilder {
    let all = h("All");
    let inner = h("Inner");
    let mut members: Vec<M> = vec![
        m(h("b"), 0x00, tc::BOOL),
        m(h("s8"), 0x01, tc::S8),
        m(h("s16"), 0x02, tc::S16),
        m(h("s32"), 0x04, tc::S32),
        m(h("u8"), 0x08, tc::U8),
        m(h("u16"), 0x0a, tc::U16),
        m(h("u32"), 0x0c, tc::U32),
        m(h("u64"), 0x10, tc::U64),
        m(h("u64b"), 0x18, tc::U64_ALT),
        m(h("half"), 0x20, tc::F16),
        m(h("kind"), 0x22, tc::ENUM8).extra(KIND),
        m(h("f"), 0x24, tc::F32),
        m(h("h"), 0x28, tc::HASH),
        m(h("h64"), 0x30, tc::HASH64),
        m(h("wide"), 0x38, tc::WIDE).sub(5),
        m(h("v3"), 0x40, tc::VEC3),
        m(h("v3p"), 0x50, tc::VEC3_PADDED),
        m(h("v4"), 0x60, tc::VEC4),
        m(h("v4b"), 0x70, tc::VEC4_ALT),
        m(h("bits"), 0x80, tc::BITSET).reference(4),
        m(h("inner"), 0x90, tc::STRUCT).extra(inner),
        m(h("ptr"), 0x98, tc::POINTER).extra(inner),
        m(h("nullptr"), 0xa0, tc::POINTER).extra(inner),
        m(h("poly"), 0xa8, tc::POINTER),
    ];
    let push_array = |members: &mut Vec<M>, elem: M, array: M| {
        let idx = members.len() as u16;
        members.push(elem);
        members.push(array.reference(idx));
    };
    push_array(
        &mut members,
        m(ANON, 0, tc::STRUCT).extra(inner),
        m(h("arr"), 0xb0, tc::ARRAY),
    );
    push_array(
        &mut members,
        m(ANON, 0, tc::U64),
        m(h("pool"), 0xc0, tc::ARRAY),
    );
    {
        let idx = members.len() as u16;
        members.push(m(ANON, 0, tc::U16));
        members.push(
            m(h("fixed"), 0xd0, tc::INLINE_ARRAY)
                .reference(idx)
                .extra(3),
        );
    }
    push_array(
        &mut members,
        m(ANON, 0, tc::HASH),
        m(h("arr52"), 0xd8, tc::ARRAY_REF16),
    );
    members.extend([
        m(h("str"), 0xe8, tc::STRING),
        m(h("strOpaque"), 0xf8, tc::STRING),
        m(h("tagged"), 0x108, tc::TAGGED),
        m(h("map"), 0x118, tc::MAP),
        m(h("hmap"), 0x130, tc::HASH_MAP),
        m(h("mystery"), 0x140, 0xee),
        m(h("u128"), 0x148, tc::WIDE).sub(0x0f),
    ]);

    let block1 = W::default()
        .u8(1)
        .u8(-5i8 as u8)
        .u16(-300i16 as u16)
        .i32(-70000)
        .u8(200)
        .u8(0)
        .u16(60000)
        .u32(4_000_000_000)
        .u64(1 << 40)
        .u64(7)
        .u16(0x3c00)
        .u8(1)
        .u8(0)
        .f32(2.5)
        .u32(h("hello"))
        .u32(0)
        .u64(0x1122_3344_5566_7788)
        .u64(9)
        .f32(1.0)
        .f32(2.0)
        .f32(3.0)
        .u32(0)
        .f32(4.0)
        .f32(5.0)
        .f32(6.0)
        .u32(0)
        .f32(1.0)
        .f32(2.0)
        .f32(3.0)
        .f32(4.0)
        .f32(5.0)
        .f32(6.0)
        .f32(7.0)
        .f32(8.0)
        .u64(0b1011)
        .u64(0)
        // 0x90 inner
        .u32(11)
        .f32(0.5)
        // 0x98 ptr, 0xa0 null, 0xa8 poly
        .u64(pool_ref(2, 8))
        .u64(0)
        .u64(pool_ref(2, 0))
        // 0xb0 arr
        .u32(2)
        .u32(2)
        .u64(pool_ref(2, 0))
        // 0xc0 pool
        .u32(3)
        .u32(3)
        .u64(pool_ref(3, 8))
        // 0xd0 fixed
        .u16(10)
        .u16(20)
        .u16(30)
        .u16(0)
        // 0xd8 arr52
        .u64(pool_ref(4, 0))
        .u16(2)
        .u16(2)
        .u32(0)
        // 0xe8 str, 0xf8 strOpaque
        .u64(pool_ref(5, 0))
        .u64(0)
        .u64(0)
        .u64(0)
        // 0x108 tagged
        .bytes(&[0x62; 16])
        // 0x118 map
        .bytes(&[0x71; 24])
        // 0x130 hmap
        .bytes(&[0x75; 16])
        // 0x140 mystery
        .bytes(&[0xee; 8])
        // 0x148 u128
        .bytes(&[0x0f; 16])
        .done();
    assert_eq!(block1.len(), 0x158);

    DocBuilder {
        structs: vec![
            S {
                name: all,
                size: 0x158,
                members,
            },
            S {
                name: inner,
                size: 8,
                members: vec![m(h("a"), 0, tc::U32), m(h("bf"), 4, tc::F32)],
            },
        ],
        enums: vec![E {
            name: KIND,
            values: vec![(0, h("first") as u64), (1, h("second") as u64)],
        }],
        blocks: vec![
            (all, block1),
            (
                inner,
                W::default().u32(21).f32(0.25).u32(22).f32(0.75).done(),
            ),
            (0x16, W::default().u64(0xdead).u64(1).u64(2).u64(3).done()),
            (0x4a, W::default().u32(h("p")).u32(h("q")).done()),
            (0x11, b"hello world\0".to_vec()),
        ],
        root: 1,
        ..Default::default()
    }
}

fn inner(a: u64, bf: f32) -> Value {
    Value::Struct {
        type_hash: hv("Inner"),
        fields: vec![(hv("a"), Value::UInt(a)), (hv("bf"), Value::Float(bf))],
    }
}

#[test]
fn decodes_every_type_code() {
    let doc = PsoDocument::parse(&all_types().build()).unwrap();
    assert!(doc.warnings().is_empty(), "{:?}", doc.warnings());
    let root = doc.root_with(&strict()).unwrap();
    let vec = |v: &[f32]| Value::Vector(v.to_vec());
    let expected = Value::Struct {
        type_hash: hv("All"),
        fields: vec![
            (hv("b"), Value::Bool(true)),
            (hv("s8"), Value::Int(-5)),
            (hv("s16"), Value::Int(-300)),
            (hv("s32"), Value::Int(-70000)),
            (hv("u8"), Value::UInt(200)),
            (hv("u16"), Value::UInt(60000)),
            (hv("u32"), Value::UInt(4_000_000_000)),
            (hv("u64"), Value::UInt(1 << 40)),
            (hv("u64b"), Value::UInt(7)),
            (hv("half"), Value::Float(1.0)),
            (
                hv("kind"),
                Value::Enum {
                    enum_hash: Hash32(KIND),
                    value: 1,
                },
            ),
            (hv("f"), Value::Float(2.5)),
            (hv("h"), Value::Hash(hv("hello"))),
            (hv("h64"), Value::Hash64(0x1122_3344_5566_7788)),
            (hv("wide"), Value::UInt(9)),
            (hv("v3"), vec(&[1.0, 2.0, 3.0])),
            (hv("v3p"), vec(&[4.0, 5.0, 6.0])),
            (hv("v4"), vec(&[1.0, 2.0, 3.0, 4.0])),
            (hv("v4b"), vec(&[5.0, 6.0, 7.0, 8.0])),
            (hv("bits"), Value::UInt(0b1011)),
            (hv("inner"), inner(11, 0.5)),
            (hv("ptr"), inner(22, 0.75)),
            (
                hv("nullptr"),
                Value::Opaque {
                    type_code: tc::POINTER,
                    bytes: vec![0; 8],
                },
            ),
            (hv("poly"), inner(21, 0.25)),
            (
                hv("arr"),
                Value::Array(vec![inner(21, 0.25), inner(22, 0.75)]),
            ),
            (
                hv("pool"),
                Value::Array(vec![Value::UInt(1), Value::UInt(2), Value::UInt(3)]),
            ),
            (
                hv("fixed"),
                Value::Array(vec![Value::UInt(10), Value::UInt(20), Value::UInt(30)]),
            ),
            (
                hv("arr52"),
                Value::Array(vec![Value::Hash(hv("p")), Value::Hash(hv("q"))]),
            ),
            (hv("str"), Value::String("hello world".into())),
            (
                hv("strOpaque"),
                Value::Opaque {
                    type_code: tc::STRING,
                    bytes: vec![0; 16],
                },
            ),
            (
                hv("tagged"),
                Value::Opaque {
                    type_code: tc::TAGGED,
                    bytes: vec![0x62; 16],
                },
            ),
            (
                hv("map"),
                Value::Opaque {
                    type_code: tc::MAP,
                    bytes: vec![0x71; 24],
                },
            ),
            (
                hv("hmap"),
                Value::Opaque {
                    type_code: tc::HASH_MAP,
                    bytes: vec![0x75; 16],
                },
            ),
            (
                hv("mystery"),
                Value::Opaque {
                    type_code: 0xee,
                    bytes: vec![0xee; 8],
                },
            ),
            (
                hv("u128"),
                Value::Opaque {
                    type_code: tc::WIDE,
                    bytes: vec![0x0f; 16],
                },
            ),
        ],
    };
    assert_eq!(root, expected);
    assert_eq!(
        root.get(hv("u32")).and_then(Value::as_u64),
        Some(4_000_000_000)
    );
    assert_eq!(
        root.get(hv("arr"))
            .and_then(Value::as_array)
            .map(<[_]>::len),
        Some(2)
    );

    // Lenient decoding gives the same tree for well-formed data.
    assert_eq!(doc.root().unwrap(), root);

    // Member sizes are exposed in the schema.
    let all = doc.schema().find_struct(hv("All")).unwrap();
    let size_of = |n: &str| all.member(hv(n)).unwrap().size;
    assert_eq!(size_of("inner"), Some(8));
    assert_eq!(size_of("fixed"), Some(6));
    assert_eq!(size_of("v3"), Some(12));
    assert_eq!(size_of("mystery"), None);

    // Instances: 1 All + 2 Inner; pool blocks skipped.
    let inst = doc.instances();
    assert_eq!(inst.len(), 3);
    assert_eq!((inst[2].block_index, inst[2].offset), (2, 8));
    assert_eq!(doc.instance_value(&inst[2]).unwrap(), inner(22, 0.75));
}

#[test]
fn enum_names_in_json() {
    let doc = PsoDocument::parse(&all_types().build()).unwrap();
    let root = doc.root().unwrap();
    let names: HashMapNames = ["All", "kind", "second"].into_iter().collect();
    let json = to_json_with_schema(&root, &names, Some(doc.schema()));
    assert_eq!(json["_type"], "All");
    assert_eq!(json["kind"], "second");
    let json = u_rome_pso::to_json(&root, &names);
    assert_eq!(json["kind"], 1);
    assert_eq!(json[format!("0x{:08X}", h("u64")).as_str()], 1u64 << 40);
}

/// Linked list node: {u32 v, pointer next}.
fn list(next: &[u64]) -> DocBuilder {
    let node = h("Node");
    let mut w = W::default();
    for (i, &n) in next.iter().enumerate() {
        w = w.u32(i as u32).u32(0).u64(n);
    }
    DocBuilder {
        structs: vec![S {
            name: node,
            size: 16,
            members: vec![
                m(h("v"), 0, tc::U32),
                m(h("next"), 8, tc::POINTER).extra(node),
            ],
        }],
        blocks: vec![(node, w.done())],
        root: 1,
        ..Default::default()
    }
}

#[test]
fn self_pointer_cycle_is_cut() {
    let doc = PsoDocument::parse(&list(&[pool_ref(1, 0)]).build()).unwrap();
    let err = doc.root_with(&strict()).unwrap_err();
    assert!(
        matches!(
            err,
            PsoError::Cycle {
                block: 1,
                offset: 0x20,
                ..
            }
        ),
        "{err}"
    );
    let v = doc.root().unwrap();
    assert_eq!(
        v.get(hv("next")),
        Some(&Value::Opaque {
            type_code: tc::POINTER,
            bytes: pool_ref(1, 0).to_le_bytes().to_vec(),
        })
    );
}

#[test]
fn two_node_cycle_is_cut() {
    let doc = PsoDocument::parse(&list(&[pool_ref(1, 16), pool_ref(1, 0)]).build()).unwrap();
    assert!(matches!(
        doc.root_with(&strict()),
        Err(PsoError::Cycle { .. })
    ));
    let v = doc.root().unwrap();
    let second = v.get(hv("next")).unwrap();
    assert_eq!(second.get(hv("v")), Some(&Value::UInt(1)));
    assert!(matches!(
        second.get(hv("next")),
        Some(Value::Opaque {
            type_code: tc::POINTER,
            ..
        })
    ));
}

#[test]
fn array_cycle_is_cut() {
    let s = h("Tree");
    let doc = DocBuilder {
        structs: vec![S {
            name: s,
            size: 16,
            members: vec![
                m(ANON, 0, tc::STRUCT).extra(s),
                m(h("kids"), 0, tc::ARRAY).reference(0),
            ],
        }],
        blocks: vec![(s, W::default().u32(1).u32(1).u64(pool_ref(1, 0)).done())],
        root: 1,
        ..Default::default()
    };
    let doc = PsoDocument::parse(&doc.build()).unwrap();
    assert!(matches!(
        doc.root_with(&strict()),
        Err(PsoError::Cycle { .. })
    ));
    let v = doc.root().unwrap();
    let kids = v.get(hv("kids")).and_then(Value::as_array).unwrap();
    assert!(matches!(
        kids[0],
        Value::Opaque {
            type_code: tc::STRUCT,
            ..
        }
    ));
}

#[test]
fn self_embedding_schema_terminates() {
    let r = h("Recursive");
    let doc = DocBuilder {
        structs: vec![S {
            name: r,
            size: 8,
            members: vec![m(h("a"), 0, tc::U32), m(h("me"), 0, tc::STRUCT).extra(r)],
        }],
        blocks: vec![(r, vec![0; 8])],
        root: 1,
        ..Default::default()
    };
    let doc = PsoDocument::parse(&doc.build()).unwrap();
    assert!(doc.root_with(&strict()).is_err());
    assert!(doc.root().is_ok());
}

fn on_thread<T: Send + 'static>(stack: usize, f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn depth_limit() {
    let n = 200;
    let next: Vec<u64> = (0..n)
        .map(|i| {
            if i + 1 < n {
                pool_ref(1, (i + 1) * 16)
            } else {
                0
            }
        })
        .collect();
    let bytes = list(&next).build();

    // Default limits on a 1 MiB thread: the depth error is returned before
    // the stack runs out. Depth errors are never swallowed in lenient mode.
    let b = bytes.clone();
    let err = on_thread(1 << 20, move || {
        PsoDocument::parse(&b).unwrap().root().unwrap_err()
    });
    assert!(
        matches!(err, PsoError::DepthLimit { limit: 64, .. }),
        "{err}"
    );

    // A raised limit decodes the whole chain.
    let len = on_thread(64 << 20, move || {
        let doc = PsoDocument::parse(&bytes).unwrap();
        let deep = DecodeOptions {
            max_depth: 500,
            ..strict()
        };
        let v = doc.root_with(&deep).unwrap();
        let mut len = 1;
        let mut cur = &v;
        while let Some(next @ Value::Struct { .. }) = cur.get(hv("next")) {
            cur = next;
            len += 1;
        }
        len
    });
    assert_eq!(len, n);
}

#[test]
fn node_limit_stops_shared_reference_blowup() {
    // 40 nodes, each pointing twice at the next: 2^40 paths if expanded.
    let d = h("Dag");
    let mut w = W::default();
    for i in 0..40u32 {
        let r = if i + 1 < 40 {
            pool_ref(1, (i + 1) * 16)
        } else {
            0
        };
        w = w.u64(r).u64(r);
    }
    let doc = DocBuilder {
        structs: vec![S {
            name: d,
            size: 16,
            members: vec![
                m(h("l"), 0, tc::POINTER).extra(d),
                m(h("r"), 8, tc::POINTER).extra(d),
            ],
        }],
        blocks: vec![(d, w.done())],
        root: 1,
        ..Default::default()
    };
    let doc = PsoDocument::parse(&doc.build()).unwrap();
    let opts = DecodeOptions {
        max_nodes: 100_000,
        ..Default::default()
    };
    let start = std::time::Instant::now();
    let err = doc.root_with(&opts).unwrap_err();
    assert!(
        matches!(err, PsoError::NodeLimit { limit: 100_000 }),
        "{err}"
    );
    assert!(start.elapsed().as_secs() < 10);
}

fn byte_array(count: u32, reference: u64, data_len: usize) -> PsoDocument {
    let s = h("Bytes");
    let b = DocBuilder {
        structs: vec![S {
            name: s,
            size: 16,
            members: vec![m(ANON, 0, tc::U8), m(h("data"), 0, tc::ARRAY).reference(0)],
        }],
        blocks: vec![
            (s, W::default().u32(count).u32(count).u64(reference).done()),
            (0x11, vec![7; data_len]),
        ],
        root: 1,
        ..Default::default()
    };
    PsoDocument::parse(&b.build()).unwrap()
}

#[test]
fn array_node_limit() {
    let doc = byte_array(1000, pool_ref(2, 0), 1000);
    let v = doc.root().unwrap();
    assert_eq!(
        v.get(hv("data")).and_then(Value::as_array).unwrap().len(),
        1000
    );
    let opts = DecodeOptions {
        max_nodes: 100,
        ..Default::default()
    };
    assert!(matches!(
        doc.root_with(&opts),
        Err(PsoError::NodeLimit { .. })
    ));
}

#[test]
fn huge_array_count_does_not_allocate() {
    let doc = byte_array(u32::MAX, pool_ref(2, 0), 16);
    let err = doc.root_with(&strict()).unwrap_err();
    assert!(matches!(err, PsoError::Malformed { .. }), "{err}");
    let v = doc.root().unwrap();
    assert!(matches!(
        v.get(hv("data")),
        Some(Value::Opaque { type_code: tc::ARRAY, bytes }) if bytes.len() == 16
    ));
}

#[test]
fn bad_references() {
    // Block index out of range.
    let doc = byte_array(2, pool_ref(9, 0), 16);
    let err = doc.root_with(&strict()).unwrap_err();
    assert!(
        matches!(err, PsoError::BadReference { reference, .. } if reference == pool_ref(9, 0)),
        "{err}"
    );
    assert!(err.to_string().contains("block index 9"));
    assert!(doc.root().is_ok());
    // Offset past the block end.
    let doc = byte_array(2, pool_ref(2, 0x1000), 16);
    assert!(matches!(
        doc.root_with(&strict()),
        Err(PsoError::BadReference { .. })
    ));
    // Elements running past the block end.
    let doc = byte_array(4, pool_ref(2, 14), 16);
    assert!(matches!(
        doc.root_with(&strict()),
        Err(PsoError::Malformed { .. })
    ));
}

#[test]
fn unknown_struct_member() {
    let s = h("Holder");
    let b = DocBuilder {
        structs: vec![S {
            name: s,
            size: 8,
            members: vec![m(h("missing"), 0, tc::STRUCT).extra(0x1234_5678)],
        }],
        blocks: vec![(s, vec![1; 8])],
        root: 1,
        ..Default::default()
    };
    let doc = PsoDocument::parse(&b.build()).unwrap();
    let err = doc.root_with(&strict()).unwrap_err();
    assert!(matches!(
        err,
        PsoError::UnknownStruct {
            hash: Hash32(0x1234_5678),
            ..
        }
    ));
    // Lenient: unknown size, so the span to the struct end is kept.
    let v = doc.root().unwrap();
    assert_eq!(
        v.get(hv("missing")),
        Some(&Value::Opaque {
            type_code: tc::STRUCT,
            bytes: vec![1; 8]
        })
    );
}

/// A miniature script.rpf DOM document (schema per rage9-re section 3).
fn script_dom_doc() -> DocBuilder {
    let root = script_dom::ROOT_STRUCT.0;
    let node = script_dom::NODE_STRUCT.0;
    let attr = script_dom::ATTRIBUTE_STRUCT.0;
    let vec3 = script_dom::VECTOR_STRUCT.0;
    let kind = script_dom::VALUE_TYPE_ENUM.0;
    let valid = 0x31a7_c9f6;
    let node_bytes = |name: u32, parent: u16, child: u16, start: u16, count: u8| {
        W::default()
            .u32(name)
            .u16(parent)
            .u16(child)
            .u16(0xffff)
            .u16(0xffff)
            .u16(start)
            .u8(count)
            .u8(1)
            .done()
    };
    let mut nodes = node_bytes(h("root"), 0xffff, 1, 0, 1);
    nodes.extend(node_bytes(h("tag"), 0, 0xffff, 1, 1));
    DocBuilder {
        structs: vec![
            S {
                name: root,
                size: 0x50,
                members: vec![
                    m(ANON, 0, tc::STRUCT).extra(attr),
                    m(h("attributes"), 0x00, tc::ARRAY).reference(0),
                    m(ANON, 0, tc::STRING),
                    m(0x5c24_11e5, 0x10, tc::ARRAY).reference(2),
                    m(ANON, 0, tc::STRUCT).extra(vec3),
                    m(0xd54b_8545, 0x20, tc::ARRAY).reference(4),
                    m(ANON, 0, tc::U64),
                    m(0x4880_f687, 0x30, tc::ARRAY).reference(6),
                    m(ANON, 0, tc::STRUCT).extra(node),
                    m(h("dataNodes"), 0x40, tc::ARRAY).reference(8),
                ],
            },
            S {
                name: node,
                size: 0x10,
                members: vec![
                    m(h("name"), 0x0, tc::HASH),
                    m(h("parentIndex"), 0x4, tc::U16),
                    m(h("childIndex"), 0x6, tc::U16),
                    m(h("siblingIndex"), 0x8, tc::U16),
                    m(h("previousSiblingIndex"), 0xa, tc::U16),
                    m(h("attributeStart"), 0xc, tc::U16),
                    m(h("attributeCount"), 0xe, tc::U8),
                    m(valid, 0xf, tc::BOOL),
                ],
            },
            S {
                name: attr,
                size: 0x10,
                members: vec![
                    m(h("name"), 0x0, tc::HASH),
                    m(h("valueHash"), 0x4, tc::U32),
                    m(0x93ba_b68e, 0x8, tc::U32),
                    m(0x2a8e_416f, 0xc, tc::ENUM8).extra(kind),
                    m(valid, 0xd, tc::BOOL),
                ],
            },
            S {
                name: vec3,
                size: 12,
                members: vec![
                    m(h("x"), 0, tc::S32),
                    m(h("y"), 4, tc::S32),
                    m(h("z"), 8, tc::S32),
                ],
            },
        ],
        enums: vec![E {
            name: kind,
            values: vec![
                (0, 0x665e_1b60),
                (1, 0x7f1e_bdfc),
                (2, 0xd685_d68f),
                (3, 0x2339_eeb0),
                (4, 0x3d39_b587),
                (5, 0xa038_df91),
            ],
        }],
        blocks: vec![
            (
                root,
                W::default()
                    .u32(2)
                    .u32(2)
                    .u64(pool_ref(2, 0))
                    .u32(0)
                    .u32(0)
                    .u64(0)
                    .u32(1)
                    .u32(1)
                    .u64(pool_ref(3, 0))
                    .u32(1)
                    .u32(1)
                    .u64(pool_ref(4, 0))
                    .u32(2)
                    .u32(2)
                    .u64(pool_ref(5, 0))
                    .done(),
            ),
            (
                attr,
                W::default()
                    .u32(h("id"))
                    .u32(h("12"))
                    .u32(12)
                    .u8(3)
                    .u8(1)
                    .u16(0)
                    .u32(h("Position"))
                    .u32(0)
                    .u32(0)
                    .u8(1)
                    .u8(1)
                    .u16(0)
                    .done(),
            ),
            (vec3, W::default().f32(1.5).f32(-2.0).f32(0.0).done()),
            (0x16, W::default().u64(0x0123_4567_89ab_cdef).done()),
            (node, nodes),
        ],
        root: 1,
        ..Default::default()
    }
}

#[test]
fn script_dom_json_snapshot() {
    let doc = PsoDocument::parse(&script_dom_doc().build()).unwrap();
    assert!(doc.warnings().is_empty(), "{:?}", doc.warnings());
    let root = doc.root_with(&strict()).unwrap();
    let names = HashMapNames::with_known_names();
    let json = to_json_with_schema(&root, &names, Some(doc.schema()));
    let text = serde_json::to_string_pretty(&json).unwrap();
    let expected = r#"{
  "_type": "0xDE396FE2",
  "attributes": [
    {
      "_type": "0x85B4C631",
      "name": "id",
      "valueHash": 1946267830,
      "0x93BAB68E": 12,
      "0x2A8E416F": "0x2339EEB0",
      "0x31A7C9F6": true
    },
    {
      "_type": "0x85B4C631",
      "name": "Position",
      "valueHash": 0,
      "0x93BAB68E": 0,
      "0x2A8E416F": "0x7F1EBDFC",
      "0x31A7C9F6": true
    }
  ],
  "0x5C2411E5": [],
  "0xD54B8545": [
    {
      "_type": "0xAEB623D8",
      "x": 1069547520,
      "y": -1073741824,
      "z": 0
    }
  ],
  "0x4880F687": [
    81985529216486895
  ],
  "dataNodes": [
    {
      "_type": "0x01393259",
      "name": "root",
      "parentIndex": 65535,
      "childIndex": 1,
      "siblingIndex": 65535,
      "previousSiblingIndex": 65535,
      "attributeStart": 0,
      "attributeCount": 1,
      "0x31A7C9F6": true
    },
    {
      "_type": "0x01393259",
      "name": "tag",
      "parentIndex": 0,
      "childIndex": 65535,
      "siblingIndex": 65535,
      "previousSiblingIndex": 65535,
      "attributeStart": 1,
      "attributeCount": 1,
      "0x31A7C9F6": true
    }
  ]
}"#;
    assert_eq!(text, expected, "actual:\n{text}");
    assert_eq!(h("12"), 1_946_267_830);
}
