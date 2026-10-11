//! uutils/coreutils 的 multicall wasm 组件 —— codemode 的「命令层」。
//!
//! 一份组件注册到多个命令名，靠 `argv[0]` 分发。宿主侧的契约（两条都必须守，否则
//! 会出现「不报错但做错事」的静默故障）：
//!
//! 1. **`argv[0]` 要是裸命令名**（`cat`，不是 `cat.wasm`、不是路径）。`uucore::util_name()`
//!    取 `argv[0]` 的 basename，错误信息 / `--help` / `--version` 都用它；
//! 2. **交给 `uumain` 的 argv 必须包含 `argv[0]`**。`#[uucore::main]` 生成的 `uumain` 把参数
//!    直接交给 clap，clap 认为第 0 个元素是程序名。少传会静默吃掉第一个真实参数
//!    （`cat FILE` 变成读 stdin，`wc -c FILE` 退化成默认三列）。
//!
//! 也支持上游那种 `coreutils <util> args…` 形式（只在本地调试时有用，宿主不必用）。
//!
//! 构建：`./generate.sh`（只需 `wasm32-wasip2` target；rustc 直出组件，不需要
//! wasi-sdk / wasm-opt / wasm-tools）。

use std::ffi::OsString;
use std::io::Write;

/// 命令登记表：加一个命令 = 这里加一行 + `Cargo.toml` 加一个依赖（见 README）。
macro_rules! utils {
    ($($name:literal => $krate:ident),* $(,)?) => {
        /// 已登记的命令名（用于「未知命令」报错与调试形式的分发）。
        const KNOWN: &[&str] = &[$($name),*];

        fn dispatch(name: &str, args: impl Iterator<Item = OsString>) -> Option<i32> {
            Some(match name {
                $($name => $krate::uumain(args),)*
                _ => return None,
            })
        }
    };
}

utils! {
    // 文本处理
    "cat" => uu_cat,
    "head" => uu_head,
    "tail" => uu_tail,
    "tac" => uu_tac,
    "nl" => uu_nl,
    "fold" => uu_fold,
    "expand" => uu_expand,
    "unexpand" => uu_unexpand,
    "od" => uu_od,
    "cut" => uu_cut,
    "tr" => uu_tr,
    "uniq" => uu_uniq,
    "comm" => uu_comm,
    "join" => uu_join,
    "paste" => uu_paste,
    "split" => uu_split,
    "shuf" => uu_shuf,
    "tsort" => uu_tsort,
    "sort" => uu_sort,
    "wc" => uu_wc,
    "seq" => uu_seq,

    // 文件 / 路径
    "ls" => uu_ls,
    "cp" => uu_cp,
    "mv" => uu_mv,
    "rm" => uu_rm,
    "mkdir" => uu_mkdir,
    "touch" => uu_touch,
    "mktemp" => uu_mktemp,
    "dirname" => uu_dirname,
    "basename" => uu_basename,
    "realpath" => uu_realpath,
    "readlink" => uu_readlink,
    "ln" => uu_ln,
    "link" => uu_link,
    "unlink" => uu_unlink,
    "truncate" => uu_truncate,

    // 杂项
    "true" => uu_true,
    "false" => uu_false,
    "echo" => uu_echo,
    "printf" => uu_printf,
    "test" => uu_test,
    "expr" => uu_expr,
    "date" => uu_date,
    "sleep" => uu_sleep,
    "tee" => uu_tee,
    "yes" => uu_yes,
    "pwd" => uu_pwd,
    "numfmt" => uu_numfmt,
    "factor" => uu_factor,
    "printenv" => uu_printenv,

    // 校验 / 编码
    "md5sum" => uu_md5sum,
    "sha1sum" => uu_sha1sum,
    "sha256sum" => uu_sha256sum,
    "sha512sum" => uu_sha512sum,
    "b2sum" => uu_b2sum,
    "cksum" => uu_cksum,
    "sum" => uu_sum,
    "base64" => uu_base64,
    "base32" => uu_base32,
    "basenc" => uu_basenc,
}

fn main() {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let argv0 = argv.first().cloned().unwrap_or_default();

    // 生产路径：宿主把 argv[0] 设成命令名。
    let mut two_arg = false;
    let mut name = base_name(&argv0);

    if !KNOWN.contains(&name.as_str()) {
        // 调试路径：`coreutils <util> args…`（上游也支持这一种）。
        let candidate = argv.get(1).map(|a| base_name(a)).unwrap_or_default();
        if KNOWN.contains(&candidate.as_str()) {
            name = candidate;
            two_arg = true;
        } else {
            eprintln!("coreutils: unknown utility '{name}'");
            eprintln!("known utilities: {}", KNOWN.join(" "));
            std::process::exit(127);
        }
    }

    let code = if two_arg {
        // uucore 的 util_name() 会去读进程 argv 的第 1 个元素。
        uucore::set_utility_is_second_arg();
        // 这一种形式下 clap 的第一个元素得是命令名，所以跳过 argv[0]（组件路径）。
        dispatch(&name, argv.into_iter().skip(1)).unwrap_or(127)
    } else {
        // 必须原样交还（含 argv[0]）：clap 认为第 0 个元素是程序名。
        dispatch(&name, std::env::args_os()).unwrap_or(127)
    };

    // 兜底 flush。std 的 exit 路径本来就会 flush（stable 1.99 / nightly 1.101 实测），
    // 这里显式再来一次，只为防将来工具链行为变化把最后一行的尾字节吞掉——代价 0。
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

/// `argv[0]` 的 basename，后缀 `.wasm` 去掉（宿主一般直接给裸名字）。
fn base_name(path: &std::ffi::OsStr) -> String {
    let base = std::path::Path::new(path)
        .file_name()
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    base.strip_suffix(".wasm").unwrap_or(&base).to_owned()
}
