//! Benchmark: Principal clone cost
//!
//! # Background
//!
//! Principal is cloned when creating Signals, elevating Sessions, etc.
//! We evaluated Arc<Principal> as an alternative but decided to keep
//! direct clone based on this benchmark.
//!
//! # Decision (2026-02)
//!
//! - Principal::User clone: 64 bytes memcpy, no heap allocation
//! - Principal::Component clone: 64 bytes + 2 heap allocations (Strings)
//! - Typical CLI session: ~100-500 clones, mostly User variant
//! - Total overhead: negligible compared to LLM API latency
//!
//! Arc<Principal> would add atomic operations without meaningful benefit
//! for this use case.
//!
//! # Benchmark Results (M1 Mac, 2026-02)
//!
//! | Operation | 200x | 500x |
//! |-----------|------|------|
//! | User clone | ~0.9 µs | ~1.1 µs |
//! | Component clone | ~60 µs | ~41 µs |
//! | Arc<User> clone | ~4 µs | ~6.7 µs |
//!
//! LLM API latency: 100ms-5s → clone overhead < 0.01%
//!
//! # When to revisit
//!
//! - If Component clones become frequent (plugin ecosystem growth)
//! - If benchmark numbers regress significantly
//! - If Principal gains more heap-allocated fields

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use orcs_types::{ComponentId, Principal, PrincipalId};
use std::sync::Arc;

fn bench_principal_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("principal_clone");

    // === Single clone benchmarks ===

    let user = Principal::User(PrincipalId::new());
    group.bench_function("User/single", |b| {
        b.iter(|| black_box(user.clone()));
    });

    let component = Principal::Component(ComponentId::new("plugin", "my-tool"));
    group.bench_function("Component/single", |b| {
        b.iter(|| black_box(component.clone()));
    });

    let system = Principal::System;
    group.bench_function("System/single", |b| {
        b.iter(|| black_box(system.clone()));
    });

    // === Arc comparison ===

    let arc_user: Arc<Principal> = Arc::new(Principal::User(PrincipalId::new()));
    group.bench_function("Arc<User>/single", |b| {
        b.iter(|| black_box(Arc::clone(&arc_user)));
    });

    let arc_component: Arc<Principal> =
        Arc::new(Principal::Component(ComponentId::new("plugin", "my-tool")));
    group.bench_function("Arc<Component>/single", |b| {
        b.iter(|| black_box(Arc::clone(&arc_component)));
    });

    group.finish();
}

fn bench_principal_clone_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("principal_clone_batch");

    // Simulate typical CLI session clone counts
    for count in [100, 200, 500] {
        group.throughput(Throughput::Elements(count as u64));

        // User variant (most common in CLI)
        let user = Principal::User(PrincipalId::new());
        group.bench_with_input(BenchmarkId::new("User", count), &count, |b, &n| {
            b.iter(|| {
                for _ in 0..n {
                    black_box(user.clone());
                }
            });
        });

        // Component variant (plugins, background tasks)
        let component = Principal::Component(ComponentId::new("plugin", "my-tool"));
        group.bench_with_input(BenchmarkId::new("Component", count), &count, |b, &n| {
            b.iter(|| {
                for _ in 0..n {
                    black_box(component.clone());
                }
            });
        });

        // Arc<Principal> for comparison
        let arc_user: Arc<Principal> = Arc::new(Principal::User(PrincipalId::new()));
        group.bench_with_input(BenchmarkId::new("Arc<User>", count), &count, |b, &n| {
            b.iter(|| {
                for _ in 0..n {
                    black_box(Arc::clone(&arc_user));
                }
            });
        });
    }

    group.finish();
}

fn bench_signal_creation_simulation(c: &mut Criterion) {
    use orcs_types::{ChannelId, SignalScope};

    // Simulate Signal creation which clones Principal
    // This represents the actual use case

    let mut group = c.benchmark_group("signal_creation_sim");

    let principal = Principal::User(PrincipalId::new());
    let channel = ChannelId::new();

    group.bench_function("signal_like_struct", |b| {
        b.iter(|| {
            // Simulates: Signal::cancel(channel, principal.clone())
            let _signal = (
                "Cancel",
                SignalScope::Channel(channel),
                black_box(principal.clone()),
            );
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_principal_clone,
    bench_principal_clone_batch,
    bench_signal_creation_simulation,
);
criterion_main!(benches);
