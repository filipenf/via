use criterion::{Criterion, black_box, criterion_group, criterion_main};

use via::ui::ghostty::layout::{
    FocusNvimAfterReference, MIN_EDITOR_PANE_COLS, PaneLayoutMode, SplitLayout,
    focus_nvim_after_agent_reference, trailing_pane_cols, vertical_split_fits,
    vertical_split_layout,
};

const SPLIT_GAP: usize = 2;

fn bench_layout(c: &mut Criterion) {
    let cell_width = 10usize;

    let mut group = c.benchmark_group("layout/vertical_split");

    // Representative window widths in "cells" (multiply by cell for pixel-ish input)
    for cols in [150, 160, 165, 200, 300] {
        let width = cols * cell_width + SPLIT_GAP;
        group.bench_with_input(format!("layout_{}c", cols), &width, |b, &w| {
            b.iter(|| {
                let layout: SplitLayout = vertical_split_layout(
                    black_box(w),
                    black_box(50),
                    black_box(cell_width),
                    black_box(Some((80, 100))),
                );
                // Touch results so they aren't optimized away
                let _ = black_box(layout.pane(0));
                let _ = black_box(layout.pane(1));
            })
        });
    }
    group.finish();

    c.bench_function("layout/fits_boundary", |b| {
        b.iter(|| {
            let _ = vertical_split_fits(
                black_box(10 * 159 + SPLIT_GAP),
                black_box(10),
                black_box(80),
            );
            vertical_split_fits(
                black_box(10 * 160 + SPLIT_GAP),
                black_box(10),
                black_box(80),
            )
        })
    });

    c.bench_function("layout/trailing_pane_cols", |b| {
        b.iter(|| {
            trailing_pane_cols(
                black_box(10 * 200 + SPLIT_GAP),
                black_box(10),
                black_box(80),
                black_box(100),
                black_box(MIN_EDITOR_PANE_COLS),
            )
        })
    });

    c.bench_function("layout/focus_after_reference", |b| {
        b.iter(|| {
            let mut mode = black_box(PaneLayoutMode::PaneMaximized(1));
            let mut active = black_box(1usize);
            let r: FocusNvimAfterReference =
                focus_nvim_after_agent_reference(black_box(&mut mode), black_box(&mut active));
            black_box((mode, active, r))
        })
    });
}

criterion_group!(benches, bench_layout);
criterion_main!(benches);
