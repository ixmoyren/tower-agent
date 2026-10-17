# coreutils-wasm (wasm component)

把 [uutils/coreutils](https://github.com/uutils/coreutils) 打成一个 **multicall** wasm 组件，
给 codemode 当「命令层」用：一份组件注册到多个命令名，宿主通过 `argv[0]` 分发。设计与边界见
[`docs/codemode.md`](../../docs/codemode.md) §3.5 / §4.1。

名字里没有 sandbox：**它不是一个沙箱，而是一个被宿主调用的 wasm 工具**（沙箱是跑脚本的那个
`wasm/rquickjs-sandbox`）。产物是 `dist/coreutils.wasm`，工具名 `coreutils`。

## 接口与宿主契约

组件本身没有自定义 WIT 接口：它就是 `wasi:cli/command`（有 `main`），靠参数分发。
宿主有两条必须守的规则，否则会出现「不报错但做错事」的静默故障：

1. **`argv[0]` 要是裸命令名**（`cat`，不是 `cat.wasm`、不是路径）。`uucore::util_name()` 取
   basename，错误信息 / `--help` / `--version` 都用它。
2. **交给 `uumain` 的 argv 必须包含 `argv[0]`**：`#[uucore::main]` 生成的 `uumain` 直接把参数
   交给 clap，clap 认为第 0 个元素是程序名。少传会静默吃掉第一个真实参数
   （`cat FILE` 变成读 stdin；`wc -c FILE` 退化成默认三列）。

本地调试还支持上游那种两参数形式 `coreutils <util> args…`（内部会调
`uucore::set_utility_is_second_arg()`），所以不用 `--argv0` 也能试：

```sh
wasmtime run dist/coreutils.wasm sort /data/f.txt
wasmtime run --argv0 cat --dir=$PWD:: dist/coreutils.wasm /data/f.txt   # 宿主那种形式
```

## 重新生成

```sh
./generate.sh              # 首次需要联网解析依赖；之后 Cargo.lock 固定，可 --offline
./generate.sh --offline
```

依赖：**只有 `wasm32-wasip2` target**（`rustup target add wasm32-wasip2`）。与 monty-sandbox
不同，这里不需要 `wasi-sdk` / `wasm-opt` / `wasm-tools`——`wasm32-wasip2` 由 rustc 直出组件。
代价是 **`wasm-opt` 处理不了组件**，体积只能靠 `[profile.release]`（`opt-level="z"` + `lto` +
`strip`）压：裸构建 11 MB → **7.4 MB**。

产物写到 `dist/coreutils.wasm`（`dist/` 与 `target/` 都不入库；内嵌还是随发行包一起放，留给
宿主 crate 那一阶段决定）。

## 命令清单（60）

| 组 | 命令 |
|----|------|
| 文本 | `cat head tail tac nl fold expand unexpand od cut tr uniq comm join paste split shuf tsort sort wc seq` |
| 文件 / 路径 | `ls cp mv rm mkdir touch mktemp dirname basename realpath readlink ln link unlink truncate` |
| 杂项 | `true false echo printf test expr date sleep tee yes pwd numfmt factor printenv` |
| 校验 / 编码 | `md5sum sha1sum sha256sum sha512sum b2sum cksum sum base64 base32 basenc` |

## 加一个命令

1. `Cargo.toml` 加依赖（`uu_xxx = "0.12"`）；
2. `src/main.rs` 的 `utils! { … }` 里加一行 `"xxx" => uu_xxx,`；
3. `./generate.sh` 编一遍——**先编再信**。

踩过的坑（决定「能不能收」）：

- **碰 `std::os::wasi` 的 util 在 `wasm32-wasip2` 上编不过**，实测两个：`du`
  （`MetadataExt::dev()/ino()`）、`env`（`std::os::wasi::ffi`）。它们要 `#![feature(wasip2)]`
  且属性必须写进**那个 crate** 的 crate 根。
- **不能用 `RUSTFLAGS=-Zcrate-attr=feature(wasip2)` 绕过**：`wasip2` 是 std 的 gate，注入到图里
  的 `#![no_std]` 依赖（`cfg-if`/`typenum`/`smallvec`/`litemap`/…）会 `E0635`。
- 因此命中这类 API 时只有两条路：**不收**，或给那**一个** crate 加一行 attribute。
- `more`（crossterm）在**任何** wasm 目标都编不过，永久排除。

## 运行期已知限制

- **符号链接建不了**：`ln -s` 报 `Permission denied`（硬链接 `ln` 正常）。属 std 的 wasip2
  支持 / wasmtime preopen 权限范畴，给模型的能力表里要注明。
- **`mv` 要求 guest 有连贯的根**：uucore 的 `canonicalize` 会逐级 `stat` 路径前缀（含 `/`），
  只 preopen 子目录时 `mv` 直接 `ENOENT`、而 `cp` 正常。宿主必须把「会话根」映射成 guest 的 `/`。
- cwasm 预编译很重要（`.wasm` 每次 spawn 重新编译 ~7 MB 模块，~141 ms；cwasm ~8 ms）。

## 许可

crates.io 上的 `uu_*` / `uucore` 各自声明 **MIT** 并带 LICENSE 文件，所以可以内嵌进本仓库的
MIT OR Apache-2.0 产物；上游仓库整体是另一套许可，正式发布前值得再确认一次。
