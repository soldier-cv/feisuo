//! 链路基线测量工具：回答"**慢的是飞梭，还是链路？**"（§9.5 基线对照）
//!
//! # 为什么需要它
//!
//! 「传一个大文件只有 3 MB/s」这句话**本身无法解读**。它可能是：
//!
//! - 链路就这个速度（家宽 24 Mbps ≈ 3 MB/s）—— 飞梭已经跑满，不是问题；
//! - 飞梭把自己限流了（socket buffer 远小于 BDP、hash 开销、提交阻塞）；
//! - 中间某个环节（ZeroTier 虚拟网卡、对方磁盘）拖后腿。
//!
//! 三种情况的处置完全相反，而**光看飞梭的速度数字区分不出来**。
//! 上一次我就是靠手工拿别的工具测出"链路上行 3.50 MB/s"，才推出
//! "飞梭 3 MB/s = 链路利用率的 86~110%"。那一次性的、靠手工的。
//!
//! 本工具把那一步固化下来，而且**用飞梭自己的 socket 配置去测** ——
//! 这样基线与实际传输同条件，比较才有意义。
//!
//! # 用法
//!
//! ```text
//! # 机器 A（被测端）
//! cargo run --release -p feisuo-core --example bench_link -- --listen
//!
//! # 机器 B（发起端）
//! cargo run --release -p feisuo-core --example bench_link -- --connect <A的IP>
//!
//! # 只在本机验证工具本身是否可用（不需要第二台机器）
//! cargo run --release -p feisuo-core --example bench_link -- --selftest
//! ```
//!
//! 建议在**真实的 ZeroTier 地址**上跑一次（`--connect 100.64.x.x`），
//! 因为那才是你的实际场景。
//!
//! # 读结果
//!
//! 工具打三行：
//!
//! | 行 | 含义 |
//! |:---|:---|
//! | `默认 buffer` | 不做任何 socket 调优时的裸 TCP 吞吐 |
//! | `飞梭 buffer` | 套用 `sockopt::tune_stream` 之后的吞吐 |
//! | 比值 | **BDP 调优买到了多少倍** —— 小于 1.1 就说明调优没用上，
//!   或者 RTT 估得太保守 |
//!
//! 然后把它和飞梭诊断里的 `纯数据流` 对比：
//! **飞梭速度 ÷ 基线速度 = 飞梭的链路利用率**。
//! - ≥ 90%：飞梭没拖后腿，慢就是链路慢（该换网络/加带宽）；
//! - 60~90%：有优化空间，看诊断里的 `verify_ms` / `commit_ms`；
//! - < 60%：飞梭侧有问题，按诊断归因查。
//!
//! 诊断报告在 `%LOCALAPPDATA%\feisuo\metrics.prom`，
//! 也可以在界面里「导出诊断报告」。

use std::time::{Duration, Instant};

use feisuo_core::transport::sockopt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 默认传输量。太小测不准（TCP 慢启动 +  ZeroTier 握手占比高），
/// 太大测试久。512 MiB 在 3 MB/s 上约 3 分钟，在 100 MB/s 上约 5 秒。
const DEFAULT_MB: usize = 512;
/// 单次写块。必须 > MSS（否则每个包一次系统调用），
/// 又不能太大（会挤掉别的连接）。
const BLOCK: usize = 256 * 1024;

/// 不可压缩的载荷。
///
/// ## 为什么不用全 0
///
/// 全 0 在 ZeroTier / 一些 VPN 上可能命中压缩或去重，得到虚高的数字；
/// 更常见的是某些 NIC 的 TSO 会让全 0 走出异常快的路径。
/// 基线一旦虚高，"飞梭跑满了吗"这个问题就答错了。
///
/// 用 xorshift 生成：足够快（不会成为瓶颈），且不可压缩。
fn payload(block_seq: u64) -> Vec<u8> {
    let mut v = vec![0u8; BLOCK];
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15 ^ block_seq.wrapping_mul(0x1234_5678_9ABC_DEF1);
    for b in v.iter_mut() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *b = (s >> 24) as u8;
    }
    v
}

fn human_mbps(bps: f64) -> String {
    if bps >= 1e9 {
        format!("{:.2} Gbps", bps / 1e9)
    } else if bps >= 1e6 {
        format!("{:.2} Mbps", bps / 1e6)
    } else {
        format!("{:.0} Kbps", bps / 1e3)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mb = parse_mb(&args).unwrap_or(DEFAULT_MB);
    let want_selftest = args.iter().any(|a| a == "--selftest");

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("无法建 runtime: {}", e);
            std::process::exit(2);
        }
    };

    let code = if want_selftest {
        rt.block_on(selftest(mb.min(64)))
    } else if args.iter().any(|a| a == "--listen") {
        rt.block_on(listen(mb))
    } else if let Some(ip) = arg_value(&args, "--connect") {
        rt.block_on(connect(&ip, mb))
    } else {
        eprintln!("用法：");
        eprintln!("  --listen              在本机监听，等待对端连接");
        eprintln!("  --connect <ip>        连接对端并测量");
        eprintln!("  --selftest            单机自检（验证工具本身可用）");
        eprintln!("  --mb <n>              传输量，默认 {DEFAULT_MB} MiB");
        2
    };
    std::process::exit(code);
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn parse_mb(args: &[String]) -> Option<usize> {
    arg_value(args, "--mb")?.parse().ok()
}

// ===========================================================================
// 服务端：把指定字节数推给对端，测**发送**方向的裸 TCP 吞吐
// ===========================================================================

async fn listen(total_mb: usize) -> i32 {
    let listener = match TcpListener::bind(("0.0.0.0", PORT)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("监听 {PORT} 失败: {e}");
            return 2;
        }
    };
    println!("基线测量服务端已就绪: 0.0.0.0:{PORT}");
    println!("让对端运行:  cargo run --release -p feisuo-core --example bench_link -- --connect <本机IP>");
    println!("（每档 {total_mb} MiB，共两档）");
    println!();

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("accept 失败: {e}");
                continue;
            }
        };
        println!("对端已连接: {peer}");
        if let Err(e) = serve_one(stream, total_mb).await {
            eprintln!("本档失败: {e}");
        }
        println!();
    }
}

/// 服务端处理**一档**。
///
/// 协议（刻意做成"一次连接一档"）：
/// 1. 客户端发 1 字节模式标记（`D` 默认 / `B` BDP 调优）
/// 2. 服务端推满 `total` 字节
/// 3. 服务端把本档 bps 以十进制文本发回
/// 4. 客户端回 1 字节 `D` 表示结束，服务端收尾
///
/// ## 为什么每一档都要**新建连接**
///
/// 否则第二档测的是"已建立很久的连接"——TCP 慢启动早就跑完了，
/// 两档不在同一条起跑线上，`默认 buffer` 与 `飞梭 buffer` 就不可比，
/// 而这个工具的全部价值就是那个比值。
async fn serve_one(mut stream: TcpStream, total_mb: usize) -> Result<(), String> {
    let mut mode = [0u8; 1];
    stream.read_exact(&mut mode).await.map_err(|e| e.to_string())?;
    let label = match mode[0] {
        b'B' => "飞梭 buffer",
        _ => "默认 buffer",
    };

    let total = (total_mb as u64) * 1024 * 1024;
    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut seq: u64 = 0;
    while sent < total {
        let chunk = payload(seq);
        seq += 1;
        stream.write_all(&chunk).await.map_err(|e| e.to_string())?;
        sent += chunk.len() as u64;
    }
    stream.flush().await.map_err(|e| e.to_string())?;
    let secs = start.elapsed().as_secs_f64().max(0.001);
    let bps = sent as f64 / secs;
    println!("服务端[{label}] 发送 {} -> {}", human_mbps(bps), sent);
    stream
        .write_all(format!("{bps:.0}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut done = [0u8; 1];
    let _ = stream.read_exact(&mut done).await;
    Ok(())
}

// ===========================================================================
// 客户端：接收并计时
// ===========================================================================

async fn connect(ip: &str, total_mb: usize) -> i32 {
    let addr = format!("{ip}:{}", PORT);
    let total = (total_mb as u64) * 1024 * 1024;
    println!("目标 {addr}，每档 {total_mb} MiB\n");

    let mut results: Vec<(String, f64)> = Vec::new();
    for (label, mode, tune) in
        [("默认 buffer", b'D', false), ("飞梭 buffer", b'B', true)]
    {
        let stream = match tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                eprintln!("连接失败: {e}");
                return 2;
            }
            Err(_) => {
                eprintln!("连接超时");
                return 2;
            }
        };
        // 连通性探测即 RTT 估计（TCP 握手约 1.5 个 RTT）
        let t0 = Instant::now();
        let s = stream;
        let _ = s.set_nodelay(true);
        let connect_ms = t0.elapsed().as_millis() as u64;

        let (mut s, est) = if tune {
            // 用**实测 RTT** 而不是配置值：估算错了基线就没有参考价值
            let est_rtt = sockopt::rtt_from_profile(
                &feisuo_core::transport::LinkProfile {
                    tcp_connect_ms: connect_ms,
                    ..Default::default()
                },
            );
            let want = sockopt::estimate_buffer(est_rtt, 0);
            // `tune_stream` 按值接收 `TcpStream`（它要 `into_std` 换手柄），
            // 所以失败时**拿不回来** —— 只能如实失败，不能"退回默认继续测"：
            // 那样测出来的就不是"飞梭配置下的速度"了，基线失去意义。
            match sockopt::tune_stream(s, want) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("socket 调优失败，无法测量「飞梭 buffer」档: {e}");
                    return 2;
                }
            }
        } else {
            (
                s,
                sockopt::BdpEstimate {
                    rtt_ms: connect_ms / 2,
                    requested: 0,
                    ..Default::default()
                },
            )
        };

        // 先告诉服务端这一档的 socket 配置，再开始收
        s.write_all(&[mode]).await.map_err(|e| e.to_string()).ok();
        let start = Instant::now();
        let mut buf = vec![0u8; BLOCK];
        let mut got: u64 = 0;
        while got < total {
            let n = match s.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("接收失败: {e}");
                    return 2;
                }
            };
            if n == 0 {
                break;
            }
            got += n as u64;
        }
        let secs = start.elapsed().as_secs_f64().max(0.001);
        let bps = got as f64 / secs;
        // 读服务端回传的它那边速度（两侧应一致；不一致说明有其它流量干扰）。
        //
        // ⚠️ **必须只读到换行，不能 `read_to_end`**：服务端写完数字后会
        // 一直等客户端的 `D` 才收尾，连接**不会**关闭。用 `read_to_end`
        // 的话两端互等，工具直接死锁 —— 而且症状是"卡住不动"，
        // 没有任何报错。
        let mut one = [0u8; 1];
        let mut txt = Vec::new();
        loop {
            match s.read(&mut one).await {
                Ok(0) => break,
                Ok(_) => {
                    if one[0] == b'\n' {
                        break;
                    }
                    txt.push(one[0]);
                    if txt.len() > 64 {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("读取服务端结果失败: {e}");
                    return 2;
                }
            }
        }
        s.write_all(b"D").await.ok();
        let srv_bps = String::from_utf8_lossy(&txt)
            .trim()
            .parse::<f64>()
            .unwrap_or(0.0);
        results.push((label.to_string(), bps));
        println!(
            "{label:<14} 客户端 {:>10} · 服务端 {:>10}   rtt≈{}ms  buffer 请求 {}KB / 生效 {}KB",
            human_mbps(bps),
            human_mbps(srv_bps),
            est.rtt_ms,
            est.requested / 1024,
            est.effective.unwrap_or(0) / 1024,
        );
    }
    println!();
    print_verdict(&results);
    0
}

fn print_verdict(results: &[(String, f64)]) {
    if results.len() < 2 {
        return;
    }
    let (a_name, a) = &results[0];
    let (b_name, b) = &results[1];
    println!("—— 结论 ——");
    println!("{a_name}: {}", human_mbps(*a));
    println!("{b_name}: {}", human_mbps(*b));
    if *a > 0.0 {
        let ratio = b / a;
        println!("BDP 调优倍数: {ratio:.2}×");
        if ratio < 1.05 {
            println!(
                "  ⚠️ 调优几乎没带来提升。要么 RTT 估得太保守、要么这条链路 RTT 很低\
                 （低 RTT 时 BDP 本来就小，256KB 已经够）。\
                 跨地域链路若出现这个结果，说明 RTT 估计有问题。"
            );
        } else {
            println!("  ✅ 调优有效：低 RTT/小窗口的链路上拿到了明显提升。");
        }
    }
    println!();
    println!("把上面的数字与飞梭诊断里的「纯数据流」对比：");
    println!("  利用率 = 飞梭速度 ÷ {}", human_mbps(results[1].1));
    println!("  ≥90% ⇒ 飞梭没拖后腿，慢就是链路慢");
    println!("  60~90% ⇒ 有优化空间，看诊断里的 verify_ms / commit_ms");
    println!("  <60% ⇒ 飞梭侧有问题，按诊断归因查");
}

/// 服务端固定端口（客户端要知道）
const PORT: u16 = 42999;

// ===========================================================================
// 单机自检：不需要第二台机器，验证工具本身没坏
// ===========================================================================

async fn selftest(total_mb: usize) -> i32 {
    println!("单机自检：在回环上跑一档 {total_mb} MiB");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定");
    let addr = listener.local_addr().expect("地址");
    let total = (total_mb as u64) * 1024 * 1024;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let start = Instant::now();
        let mut sent = 0u64;
        let mut seq = 0u64;
        while sent < total {
            let chunk = payload(seq);
            seq += 1;
            if s.write_all(&chunk).await.is_err() {
                break;
            }
            sent += chunk.len() as u64;
        }
        let _ = s.flush().await;
        sent as f64 / start.elapsed().as_secs_f64().max(0.001)
    });

    let mut s = TcpStream::connect(addr).await.expect("连接");
    let mut buf = vec![0u8; BLOCK];
    let start = Instant::now();
    let mut got = 0u64;
    while got < total {
        let n = s.read(&mut buf).await.expect("读");
        if n == 0 {
            break;
        }
        got += n as u64;
    }
    let secs = start.elapsed().as_secs_f64().max(0.001);
    let bps = got as f64 / secs;
    let srv = server.await.expect("join");

    println!("收到 {got} 字节，耗时 {secs:.3}s");
    println!("客户端视角: {}", human_mbps(bps));
    println!("服务端视角: {}", human_mbps(srv));
    if got != total {
        println!("❌ 字节数不符（收到 {got}，应为 {total}）—— 工具本身有问题");
        return 1;
    }
    // 两侧计数应一致（差一个 block 以内的误差是计时点不同造成的）
    if (got as f64 / srv - 1.0).abs() > 0.05 {
        println!("⚠️ 两侧速度差超过 5%，请检查是否有其它流量干扰");
    }
    println!("✅ 工具自检通过（这只证明工具可用，**不代表你的链路性能**）");
    0
}
