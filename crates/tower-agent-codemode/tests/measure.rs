//! 量一下 codemode 的时间都花在哪。
//!
//! ```sh
//! cargo test -p tower-agent-codemode --offline --test measure -- --ignored --nocapture
//! ```

use std::time::Instant;

use tower_agent_codemode::{CodemodeHost, CodemodeOptions};

#[test]
#[ignore = "手动跑：打印各组件的编译/实例化耗时"]
fn measure_component_costs() {
    // wasmtime 同步调用用调用方线程的栈（见 docs §3.1），所以在大栈线程上跑。
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let dir = tempfile::tempdir().expect("tmp");
            std::fs::write(dir.path().join("f.txt"), "b\na\n").expect("fixture");

            let started = Instant::now();
            let host = CodemodeHost::new(CodemodeOptions::embedded(dir.path())).expect("host");
            println!(
                "[构造] CodemodeHost::new（编译 coreutils 7.4 MB + guest 0.8 MB）: {:?}",
                started.elapsed()
            );

            let started = Instant::now();
            host.run("return 1 + 1", None).expect("run");
            println!("[热]   脚本（不含命令）: {:?}", started.elapsed());

            let started = Instant::now();
            host.run(r#"return await cat("/f.txt")"#, None)
                .expect("run");
            println!("[热]   脚本 + 1 条命令: {:?}", started.elapsed());

            let started = Instant::now();
            host.run(
                r#"for (let i = 0; i < 5; i++) { await cat("/f.txt") } return 0"#,
                None,
            )
            .expect("run");
            println!("[热]   脚本 + 5 条命令: {:?}", started.elapsed());

            let started = Instant::now();
            let second = CodemodeHost::new(CodemodeOptions::embedded(dir.path())).expect("host");
            println!(
                "[构造] 第二个 host（JIT 缓存应当命中）: {:?}",
                started.elapsed()
            );
            drop(second);
        })
        .expect("spawn")
        .join()
        .expect("join");
}
