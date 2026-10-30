//! Hand-assembled minimal hostile modules (bytes only, never executed by
//! tests except inside the validator / browser rejection smoke).

fn sec(id: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![id, body.len() as u8];
    v.extend_from_slice(body);
    v
}

fn module(sections: &[Vec<u8>]) -> Vec<u8> {
    let mut v = b"\0asm\x01\0\0\0".to_vec();
    for s in sections {
        v.extend_from_slice(s);
    }
    v
}

const TYPE_VOID: &[u8] = &[1, 0x60, 0, 0];

/// `(name, bytes, expected host error code)`
pub fn all() -> Vec<(&'static str, Vec<u8>, &'static str)> {
    let mem = |flags: u8| sec(5, &[1, flags, 1, 1]);
    // valid bounded memory (1..=1 pages) used by table/limit cases
    let bmem = sec(5, &[1, 1, 1, 1]);
    let export_mem = sec(7, &[1, 6, b'm', b'e', b'm', b'o', b'r', b'y', 2, 0]);
    vec![
        ("import", module(&[sec(1, TYPE_VOID), sec(2, &[1, 3, b'e', b'n', b'v', 1, b'f', 0, 0])]), "forbidden_import"),
        (
            "start",
            module(&[sec(1, TYPE_VOID), sec(3, &[1, 0]), sec(5, &[1, 0, 1]), sec(8, &[0]), sec(10, &[1, 2, 0, 0x0b])]),
            "forbidden_start",
        ),
        ("shared_memory", module(&[mem(0x03)]), "bad_memory"),
        ("memory64", module(&[sec(5, &[1, 0x04, 1])]), "bad_memory"),
        ("huge_memory", module(&[sec(5, &[1, 0, 0x80, 0x10])]), "bad_memory"),
        ("missing_exports", module(&[bmem.clone(), export_mem]), "missing_export"),
        // --- parent review #1 counterexamples (rejected before compile) ---
        ("unbounded_memory", module(&[sec(5, &[1, 0, 1])]), "bad_memory"),
        ("over_max_memory", module(&[sec(5, &[1, 1, 1, 0x81, 0x40])]), "bad_memory"),
        ("growable_table", module(&[sec(4, &[1, 0x70, 0, 1]), bmem.clone()]), "bad_table"),
        ("huge_table", module(&[sec(4, &[1, 0x70, 1, 1, 0x81, 0x80, 0x04]), bmem.clone()]), "bad_table"),
        ("two_tables", module(&[sec(4, &[2, 0x70, 1, 1, 1, 0x70, 1, 1, 1]), bmem.clone()]), "resource_limit"),
        ("externref_table", module(&[sec(4, &[1, 0x6f, 1, 1, 1]), bmem.clone()]), "bad_table"),
        ("type_count_bomb", module(&[sec(1, &[0xff, 0xff, 0xff, 0x0f])]), "resource_limit"),
        ("func_count_bomb", module(&[sec(1, TYPE_VOID), sec(3, &[0x80, 0x80, 0x80, 0x01])]), "resource_limit"),
        ("bad_magic", b"\0asn\x01\0\0\0".to_vec(), "bad_magic"),
        ("truncated", module(&[vec![1, 9, 1]]), "malformed_module"),
    ]
}
