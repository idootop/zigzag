//! 一次性实验：拿真实素材跑完整扫描，看报告数字合不合理。验完即删。
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    zigzag_lib::logging::init();
    let dbp = std::path::Path::new("/tmp/zzscan.db");
    let _ = std::fs::remove_file(dbp);
    let db = Arc::new(zigzag_lib::store::Db::open(dbp).unwrap());
    let roots = vec!["/tmp/zzimg".into(), "/tmp/zzprobe".into()];

    let t = std::time::Instant::now();
    let mut ticks = 0;
    let r = zigzag_lib::scan::run(
        db,
        zigzag_lib::config::Profile::default(),
        roots,
        Arc::new(AtomicBool::new(false)),
        |p| {
            ticks += 1;
            if p.done {
                println!("[进度] 收尾 analyzed={} media={}", p.analyzed, p.media_found);
            }
        },
    )
    .await;
    let el = t.elapsed();

    let mb = |b: f64| b / 1e6;
    println!("\n耗时 {el:?}，进度事件 {ticks} 条");
    println!("扫到 {} 个文件，其中媒体 {}，错误 {}", r.files_seen, r.media_found, r.errors);
    println!(
        "待处理 {} 个 / {:.1} MB → {:.1} MB（{:.1}~{:.1}）",
        r.planned_files,
        mb(r.planned_bytes as f64),
        mb(r.out_bytes.mid),
        mb(r.out_bytes.low),
        mb(r.out_bytes.high)
    );
    println!(
        "可省 {:.1} MB（{:.1}~{:.1}），耗时 {:.1}s（{:.1}~{:.1}） cpu={:.1}s hw={:.1}s",
        mb(r.saved_bytes.mid),
        mb(r.saved_bytes.low),
        mb(r.saved_bytes.high),
        r.seconds.mid,
        r.seconds.low,
        r.seconds.high,
        r.cpu_seconds.mid,
        r.hw_seconds.mid
    );
    println!("\n按类型:");
    for g in &r.groups {
        println!(
            "  {:?}: {} 个 {:.1} MB → {:.1} MB, {:.1}s",
            g.kind,
            g.files,
            mb(g.src_bytes as f64),
            mb(g.out_bytes.mid),
            g.seconds.mid
        );
    }
    println!("\n跳过 {} 个 / {:.1} MB:", r.skipped_files, mb(r.skipped_bytes as f64));
    for s in &r.skipped {
        println!("  {:?} × {} ({:.1} MB) — {}", s.reason, s.files, mb(s.bytes as f64), s.message);
    }
    println!("\n目录分布:");
    for d in &r.dirs {
        println!("  {:<12} {} 个 {:.1} MB", d.name, d.files, mb(d.bytes as f64));
    }

    // 第二次跑：probe_cache 应当全命中，明显更快。
    let db2 = Arc::new(zigzag_lib::store::Db::open(dbp).unwrap());
    let t2 = std::time::Instant::now();
    let r2 = zigzag_lib::scan::run(
        db2,
        zigzag_lib::config::Profile::default(),
        vec!["/tmp/zzimg".into(), "/tmp/zzprobe".into()],
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .await;
    println!("\n第二次扫描 {:?}（首次 {el:?}），结果一致={}", t2.elapsed(), r2 == r);
    let _ = std::fs::remove_file(dbp);
}
