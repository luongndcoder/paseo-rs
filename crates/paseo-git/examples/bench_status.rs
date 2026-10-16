//! Microbench: `get_checkout_status` on a repo. `cargo run --release -p paseo-git --example bench_status -- <repo>`
use std::time::Instant;

use paseo_git::repo::CheckoutContext;
use paseo_git::status::get_checkout_status;

#[tokio::main]
async fn main() {
    let repo = std::env::args().nth(1).expect("repo path");
    let ctx = CheckoutContext::default();
    for _ in 0..3 {
        get_checkout_status(&repo, &ctx).await.unwrap();
    }
    let mut samples = Vec::new();
    for _ in 0..30 {
        let t = Instant::now();
        get_checkout_status(&repo, &ctx).await.unwrap();
        samples.push(t.elapsed().as_secs_f64() * 1e3);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{{\"impl\":\"rust\",\"ms_p50\":{:.1},\"ms_p90\":{:.1}}}",
        samples[15], samples[27]
    );
}
