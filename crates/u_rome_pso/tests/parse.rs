//! Container-level tests: sections, schema tables, block table, footer and
//! malformed framing. All documents are synthetic.

mod common;

use common::{DocBuilder, E, S, W, m, section};
use u_rome_pso::{
    BlockCountSource, Hash32, Layout, PsoDocument, PsoError, RootSource, type_code as tc,
};

const ROOT: u32 = 0xde39_6fe2;
const NODE: u32 = 0x0139_3259;
const KIND: u32 = 0x649e_cf8e;

fn small() -> DocBuilder {
    DocBuilder {
        structs: vec![
            S {
                name: ROOT,
                size: 0x10,
                members: vec![
                    m(0x100, 0, tc::STRUCT).extra(NODE),
                    m(0x2260_b8be, 0, tc::ARRAY).reference(0),
                ],
            },
            S {
                name: NODE,
                size: 8,
                members: vec![
                    m(0xe6e1_7f14, 0, tc::HASH),
                    m(0x2a8e_416f, 4, tc::ENUM8).extra(KIND),
                ],
            },
        ],
        enums: vec![E {
            name: KIND,
            values: vec![(0, 0x665e_1b60), (1, 0x7f1e_bdfc)],
        }],
        blocks: vec![
            (
                ROOT,
                W::default()
                    .u32(2)
                    .u32(2)
                    .u64(common::pool_ref(2, 0))
                    .done(),
            ),
            (
                NODE,
                W::default()
                    .u32(0xc090_afad)
                    .u8(0)
                    .pad_to(8)
                    .u32(0x1aec_de4d)
                    .u8(1)
                    .pad_to(16)
                    .done(),
            ),
        ],
        root: 1,
        ..Default::default()
    }
}

#[test]
fn parses_sections_schema_and_blocks() {
    let bytes = small().build();
    let doc = PsoDocument::parse(&bytes).unwrap();
    let tags: Vec<&str> = doc.sections().iter().map(|s| s.tag_str()).collect();
    assert_eq!(
        tags,
        ["PSIN", "LSCH", "LENM", "LSNS", "LGID", "LRSC", "CHKS"]
    );
    assert_eq!(doc.sections()[0].offset, 0);
    let last = doc.sections().last().unwrap();
    assert_eq!(last.end(), bytes.len());
    assert_eq!(doc.layout(), Layout::Lsch);
    assert!(doc.warnings().is_empty(), "{:?}", doc.warnings());

    let schema = doc.schema();
    assert_eq!(schema.structs.len(), 2);
    let root = schema.find_struct(Hash32(ROOT)).unwrap();
    assert_eq!(root.size, 0x10);
    assert_eq!(root.signature, 0x5151_0000);
    assert_eq!(root.name64(), 0x5151_0000_de39_6fe2);
    assert_eq!(root.members[1].type_code, tc::ARRAY);
    assert_eq!(root.members[1].size, Some(16));
    assert_eq!(root.members[0].size, Some(8), "struct size from schema");
    assert_eq!(root.fields().count(), 1);
    assert_eq!(root.element_of(1).unwrap().extra, NODE);
    let e = schema.find_enum(Hash32(KIND)).unwrap();
    assert_eq!(e.values.len(), 2);
    assert_eq!(
        schema.enum_value_name(Hash32(KIND), 1),
        Some(Hash32(0x7f1e_bdfc))
    );

    assert_eq!(doc.blocks().len(), 2);
    assert_eq!(doc.block_count_source(), Some(BlockCountSource::Lrsc));
    let b1 = doc.block(1).unwrap();
    assert_eq!(b1.offset % 16, 0);
    assert_eq!(b1.offset, 0x30, "8 + 2 * 16 rounded up to 16");
    assert_eq!(doc.block_data(2).unwrap().len(), 16);
    assert!(doc.block(0).is_none());
    assert!(doc.block(3).is_none());

    let hdr = doc.resource_header().unwrap();
    assert_eq!(hdr.version, 0x7a);
    assert_eq!(hdr.root_block_index, 1);
    let root = doc.root_info().unwrap();
    assert_eq!(root.source, RootSource::LrscRootField);
    assert!(!root.provisional);

    let lsns = doc.lsns().unwrap();
    assert_eq!((lsns.count, lsns.flags), (1, 0x101));
    assert_eq!(lsns.table_lengths, [16]);
    assert!(lsns.framing_ok);
    let chks = doc.checks().unwrap();
    assert!(chks.size_matches);
    assert_eq!(chks.checksum, 0x1234_5678);
    assert_eq!(&chks.tag, b"zppp");
    assert_eq!(doc.section_body(doc.section(b"LGID").unwrap()).len(), 0);
}

#[test]
fn rejects_non_psin() {
    assert!(matches!(
        PsoDocument::parse(b"RDS5xxxx"),
        Err(PsoError::NotPsin { .. })
    ));
    assert!(matches!(
        PsoDocument::parse(b""),
        Err(PsoError::NotPsin { .. })
    ));
}

#[test]
fn malformed_section_sizes() {
    let good = small().build();
    let lsch = PsoDocument::parse(&good).unwrap().sections()[1].offset;

    // Size smaller than the header.
    let mut b = good.clone();
    b[lsch + 4..lsch + 8].copy_from_slice(&4u32.to_be_bytes());
    let err = PsoDocument::parse(&b).unwrap_err();
    assert!(
        matches!(err, PsoError::BadSectionSize { offset, .. } if offset == lsch),
        "{err}"
    );

    // Size past the end.
    let mut b = good.clone();
    b[lsch + 4..lsch + 8].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
    let err = PsoDocument::parse(&b).unwrap_err();
    assert!(err.to_string().contains("LSCH"), "{err}");
    assert!(matches!(err, PsoError::BadSectionSize { .. }));

    // Size that lands mid-section, so the next tag is garbage.
    let mut b = good.clone();
    let size = u32::from_be_bytes(b[lsch + 4..lsch + 8].try_into().unwrap());
    b[lsch + 4..lsch + 8].copy_from_slice(&(size - 3).to_be_bytes());
    assert!(PsoDocument::parse(&b).is_err());

    // PSIN size of u32::MAX.
    let mut b = good.clone();
    b[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        PsoDocument::parse(&b),
        Err(PsoError::BadSectionSize { .. })
    ));

    // Truncated in the middle of a header.
    assert!(PsoDocument::parse(&good[..good.len() - 17]).is_err());
    // Every truncation either parses or errors, never panics.
    for n in 0..good.len() {
        let _ = PsoDocument::parse(&good[..n]);
    }
}

#[test]
fn garbage_never_panics() {
    let good = small().build();
    let mut seed = 0x1234_5678u32;
    for _ in 0..2000 {
        let mut b = good.clone();
        for _ in 0..4 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let i = seed as usize % b.len();
            b[i] = (seed >> 8) as u8;
        }
        if let Ok(doc) = PsoDocument::parse(&b) {
            let _ = doc.root();
            for inst in doc.instances() {
                let _ = doc.instance_value(&inst);
            }
        }
    }
}

#[test]
fn unknown_sections_are_kept_opaque() {
    let mut b = small();
    b.extra_sections.push((*b"ZZZZ", vec![1, 2, 3, 4, 5]));
    let doc = PsoDocument::parse(&b.build()).unwrap();
    let z = doc.section(b"ZZZZ").unwrap();
    assert_eq!(doc.section_body(z), [1, 2, 3, 4, 5]);
    assert!(doc.checks().unwrap().size_matches);
}

#[test]
fn trailing_zero_padding_is_tolerated() {
    let mut bytes = small().build();
    bytes.extend_from_slice(&[0; 13]);
    let doc = PsoDocument::parse(&bytes).unwrap();
    assert_eq!(doc.sections().len(), 7);
    assert!(doc.checks().unwrap().size_matches);
    assert_eq!(doc.warnings().len(), 1);
}

#[test]
fn chks_size_mismatch_is_reported() {
    let mut b = small();
    b.chks_size_delta = 16;
    let doc = PsoDocument::parse(&b.build()).unwrap();
    assert!(!doc.checks().unwrap().size_matches);
    assert!(doc.warnings().iter().any(|w| w.contains("CHKS")));
}

#[test]
fn block_count_fallbacks() {
    let mut b = small();
    b.lrsc_block_count = Some(7);
    let doc = PsoDocument::parse(&b.build()).unwrap();
    assert_eq!(doc.blocks().len(), 2);
    assert_eq!(doc.block_count_source(), Some(BlockCountSource::PoisonRun));
    assert!(!doc.warnings().is_empty());
}

#[test]
fn lrsc_enum_count_disagreement_falls_back() {
    // GTA6-Asset-Info reads LRSC +0x3A as the root block; put a 2 there.
    let mut b = small();
    b.lrsc_enum_count = Some(2);
    let doc = PsoDocument::parse(&b.build()).unwrap();
    assert_eq!(doc.schema().enums.len(), 1);
    assert!(doc.warnings().iter().any(|w| w.contains("+0x3A")));
}

#[test]
fn root_fallback_is_provisional() {
    let mut b = small();
    b.root = 9;
    b.lrsc_enum_count = Some(1);
    let doc = PsoDocument::parse(&b.build()).unwrap();
    let root = doc.root_info().unwrap();
    assert!(root.provisional);
    assert_eq!(root.block_index, 1);
    assert_eq!(root.source, RootSource::LrscAltField);

    let mut b = small();
    b.root = 0;
    b.lrsc_enum_count = Some(1);
    b.enums.clear();
    b.lrsc_enum_count = Some(0);
    let doc = PsoDocument::parse(&b.build()).unwrap();
    let root = doc.root_info().unwrap();
    assert_eq!(root.source, RootSource::Unreferenced);
    assert_eq!(root.block_index, 1);
}

#[test]
fn pmap_layout_is_opaque() {
    let mut out = Vec::new();
    section(&mut out, b"PSIN", &[0u8; 24]);
    section(&mut out, b"PMAP", &[1u8; 12]);
    section(&mut out, b"PSCH", &[2u8; 8]);
    section(&mut out, b"PSIG", &[3u8; 4]);
    section(&mut out, b"STFN", &[4u8; 4]);
    section(&mut out, b"STNS", &[5u8; 4]);
    let total = (out.len() + 20) as u32;
    let mut chks = total.to_be_bytes().to_vec();
    chks.extend_from_slice(&[0, 0, 0, 1]);
    chks.extend_from_slice(b"zppp");
    section(&mut out, b"CHKS", &chks);

    let doc = PsoDocument::parse(&out).unwrap();
    assert_eq!(doc.layout(), Layout::Pmap);
    assert_eq!(doc.sections().len(), 7);
    assert!(doc.blocks().is_empty());
    assert!(doc.schema().structs.is_empty());
    assert_eq!(doc.section_body(doc.section(b"PMAP").unwrap()), [1u8; 12]);
    assert!(doc.checks().unwrap().size_matches);
    assert!(matches!(doc.root(), Err(PsoError::Unsupported(_))));
}
