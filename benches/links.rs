use std::path::Path;

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};

use via::ui::ghostty::links::{reference_target_from_row, reference_target_from_uri};

/// A small corpus of realistic-ish agent output lines containing file refs, symbols,
/// URIs, and noise. We scan "a screen" or a transcript worth of these.
fn sample_lines() -> Vec<&'static str> {
    vec![
        "diff --git a/src/main.rs b/src/main.rs",
        "index 1234567..89abcde 100644",
        "--- a/src/main.rs",
        "+++ b/src/main.rs",
        "@@ -10,6 +10,7 @@ fn foo() {",
        "     let x = 1;",
        "     // see `Bar::baz` for details",
        "     open src/ui/ghostty/layout.rs:42",
        "     also look at crates/foo/src/lib.rs:7:13",
        "     symbol://Vec%3A%3Anew",
        "     call the (`Parser::parse`) helper",
        "     more text here with no refs 123",
        "     path/to/some/deep/module.rs",
        "     error at target/debug/build/xxx/out/foo.rs:9",
        "     another `some_crate::thing::Sub` mention",
        "     https://example.com/not/a/ref",
        "     file with (parens)/weird-name_1.2.rs:100",
        "     end of block",
    ]
}

fn bench_links(c: &mut Criterion) {
    let wd = Path::new("/repo");

    let lines = sample_lines();
    let num_lines = lines.len();

    let mut group = c.benchmark_group("links/scan");
    group.throughput(Throughput::Elements(num_lines as u64));

    group.bench_function("reference_target_from_row_many", |b| {
        b.iter(|| {
            for (i, line) in lines.iter().enumerate() {
                // Probe near a plausible click column (varies a bit)
                let col = 8 + (i % 5);
                let _ = black_box(reference_target_from_row(
                    black_box(line),
                    black_box(col),
                    black_box(wd),
                ));
            }
        })
    });
    group.finish();

    c.bench_function("links/reference_target_from_uri_symbol", |b| {
        b.iter(|| {
            black_box(reference_target_from_uri(
                black_box("symbol://Foo%3A%3Abar_baz"),
                black_box(wd),
            ))
        })
    });

    c.bench_function("links/reference_target_from_uri_file", |b| {
        b.iter(|| {
            black_box(reference_target_from_uri(
                black_box("file:///repo/src/main.rs#L10-L20"),
                black_box(wd),
            ))
        })
    });

    // Also exercise a miss path (common)
    c.bench_function("links/reference_target_from_row_miss", |b| {
        b.iter(|| {
            black_box(reference_target_from_row(
                black_box("just some plain prose with 123 numbers and no paths"),
                black_box(10),
                black_box(wd),
            ))
        })
    });
}

criterion_group!(benches, bench_links);
criterion_main!(benches);
