//! 构建脚本: 链接参数 + libopus (fixed-point) C 交叉编译
//!
//! `vendor/libopus` 是 xiph/opus v1.6.1 的裁剪副本 —— 仅保留 fixed-point
//! 构建所需源码 (无 float/dnn/x86/ARM), 许可证见 `vendor/libopus/COPYING`。
//! C 编译器为 espup 安装的 xtensa GCC, 按以下顺序探测:
//! 1. 环境变量 `CC_xtensa_esp32s3_none_elf` / `CC`
//! 2. PATH 中的 `xtensa-esp32s3-elf-gcc`
//! 3. espup 默认安装目录 `~/.espressif/tools/xtensa-esp-elf/*/xtensa-esp-elf/bin`

use std::env;
use std::path::PathBuf;

const OPUS_DIR: &str = "vendor/libopus";
const XTENSA_GCC: &str = "xtensa-esp32s3-elf-gcc";

fn main() {
    linker_be_nice();
    println!("cargo:rustc-link-arg=-Tdefmt.x");
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    build_libopus();
}

/// 编译 libopus C 源码为静态库 (仅固件目标)
fn build_libopus() {
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("xtensa-esp32s3") {
        return;
    }
    let compiler = find_xtensa_gcc().unwrap_or_else(|| {
        panic!(
            "找不到 {XTENSA_GCC}: 请安装 espup 工具链 (含 GCC), \
             或设置环境变量 CC={XTENSA_GCC}"
        )
    });

    let mut build = cc::Build::new();
    build
        .compiler(&compiler)
        .warnings(false)
        .opt_level_str("s")
        .flag("-mlongcalls") // xtensa 代码段跨度大, 必须长调用
        .define("OPUS_BUILD", None)
        .define("FIXED_POINT", None) // 定点运算: 嵌入式官方配置, 性能/内存最优
        .define("DISABLE_FLOAT_API", None)
        .define("VAR_ARRAYS", None) // 内部 scratch 用 C99 VLA (走任务栈)
        .define("PACKAGE_VERSION", Some("\"1.6.1\""))
        .include(format!("{OPUS_DIR}/include"))
        .include(format!("{OPUS_DIR}/src"))
        .include(format!("{OPUS_DIR}/celt"))
        .include(format!("{OPUS_DIR}/silk"))
        .include(format!("{OPUS_DIR}/silk/fixed"));

    // 源码清单与 xiph 官方 .mk 一致: 全部 celt + 全部 silk + silk/fixed +
    // src 核心 5 文件 (multistream/projection/analysis/mlp 不在链路上)
    for dir in ["src", "celt", "silk", "silk/fixed"] {
        for entry in std::fs::read_dir(format!("{OPUS_DIR}/{dir}")).expect("vendor/libopus 缺失")
        {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "c") {
                build.file(path);
            }
        }
    }

    build.compile("opus");
    println!("cargo:rerun-if-changed={OPUS_DIR}");
}

/// 按文件头注释的顺序探测 xtensa GCC
fn find_xtensa_gcc() -> Option<String> {
    for var in ["CC_xtensa_esp32s3_none_elf", "CC"] {
        if let Ok(cc) = env::var(var) {
            if !cc.is_empty() {
                return Some(cc);
            }
        }
    }
    if which(XTENSA_GCC).is_some() {
        return Some(XTENSA_GCC.to_string());
    }
    // espup 默认安装布局, 取版本号最大的目录
    let home = env::var("HOME").ok()?;
    let pattern =
        format!("{home}/.espressif/tools/xtensa-esp-elf/*/xtensa-esp-elf/bin/{XTENSA_GCC}");
    let mut candidates = glob_walk(&pattern);
    candidates.sort();
    candidates.pop().map(|p| p.display().to_string())
}

/// 极简 PATH 查找 (避免引入 which crate)
fn which(name: &str) -> Option<PathBuf> {
    let paths = env::var("PATH").ok()?;
    for dir in paths.split(':') {
        let p = PathBuf::from(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 展开含单个 `*` 通配的路径模式 (只匹配一层目录, 避免引入 glob crate)
fn glob_walk(pattern: &str) -> Vec<PathBuf> {
    let Some((head, tail)) = pattern.split_once('*') else {
        return vec![PathBuf::from(pattern)];
    };
    let (parent, prefix) = match head.rsplit_once('/') {
        Some((p, x)) => (p, x),
        None => (".", head),
    };
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(prefix) {
            continue;
        }
        let joined = PathBuf::from(format!("{parent}/{name}{tail}"));
        if joined.is_file() {
            out.push(joined);
        }
    }
    out
}

fn linker_be_nice() {
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                what if what.starts_with("_defmt_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `defmt` not found - make sure `defmt.x` is added as a linker script and you have included `use defmt_rtt as _;`"
                    );
                    eprintln!();
                },
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                },
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `esp-radio` has no scheduler enabled. Make sure you have initialized `esp-rtos` or provided an external scheduler."
                    );
                    eprintln!();
                },
                "free"
                | "malloc"
                | "calloc"
                | "get_free_internal_heap_size"
                | "malloc_internal"
                | "realloc_internal"
                | "calloc_internal"
                | "free_internal" => {
                    eprintln!();
                    eprintln!(
                        "💡 Did you forget the `esp-alloc` dependency or didn't enable the `compat` feature on it?"
                    );
                    eprintln!();
                },
                _ => (),
            },
            // we don't have anything helpful for "missing-lib" yet
            _ => {
                std::process::exit(1);
            },
        }
        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
        env::current_exe().unwrap().display()
    );
}
