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

    if env::var_os("CARGO_FEATURE_WAKENET").is_some() {
        build_wakenet(&compiler);
    }
}

/// esp-sr v1.9.5 (feature `wakenet`): 预编译库链接 + 环境垫片
fn build_wakenet(compiler: &str) {
    let sr = "vendor/esp-sr";
    // 1) 环境垫片 (model_path/文件 IO/日志/队列桩)
    let mut shim = cc::Build::new();
    shim.compiler(compiler)
        .warnings(false)
        .opt_level_str("s")
        .flag("-mlongcalls")
        .include(format!("{sr}/include"))
        .include("vendor/cjson")
        .file(format!("{sr}/src/model_path_shim.c"));
    shim.compile("esp_sr_shim");

    // cJSON (dl_lib_coefgetter_if.h 头引用)
    let mut cjson = cc::Build::new();
    cjson
        .compiler(compiler)
        .warnings(false)
        .opt_level_str("s")
        .flag("-mlongcalls")
        .include("vendor/cjson")
        .file("vendor/cjson/cJSON.c");
    cjson.compile("cjson_wrap");

    // 2) esp-dsp 子集 (MFCC 所需 FFT/点积; C + Xtensa 汇编)
    //    源码: vendor/esp-dsp (v1.5.2 子集)
    let dsp = "vendor/esp-dsp";
    let mut dsp_build = cc::Build::new();
    dsp_build
        .compiler(compiler)
        .warnings(false)
        .opt_level_str("s")
        .flag("-mlongcalls")
        .include(format!("{dsp}/modules/fft/include"))
        .include(format!("{dsp}/modules/dotprod/include"))
        .include(format!("{dsp}/modules/common/include"))
        .include(format!("{dsp}/compat"))
        .include(format!("{dsp}/compat/xtensa/config"));
    let dsp_files = [
        "modules/fft/float/dsps_fft2r_fc32_ansi.c",
        "modules/fft/float/dsps_fft4r_fc32_ansi.c",
        "modules/fft/float/dsps_fft2r_bitrev_tables_fc32.c",
        "modules/fft/float/dsps_fft4r_bitrev_tables_fc32.c",
        "modules/dotprod/float/dsps_dotprod_f32_ansi.c",
        "modules/support/support_func_c.c",
        "compat/esp_dsp_compat.c",
    ];
    for f in dsp_files {
        let path = format!("{dsp}/{f}");
        if std::path::Path::new(&path).exists() {
            dsp_build.file(path);
        }
    }
    dsp_build.compile("esp_dsp_subset");

    // 3) 预编译库 (顺序敏感: wakenet 最后; group 允许循环引用)
    println!("cargo:rustc-link-search=native={sr}/lib");
    println!("cargo:rustc-link-lib=static=c_speech_features");
    println!("cargo:rustc-link-lib=static=dl_lib");
    println!("cargo:rustc-link-lib=static=hufzip");
    println!("cargo:rustc-link-lib=static=wakenet");
    println!("cargo:rerun-if-changed={sr}");

    // 数学函数 (libm; libc 的 ssp/stdio 与固件冲突, 缺口全部走 shim)
    println!("cargo:rustc-link-lib=m");
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
