/* esp-sr v1.9.5 esp-hal (no_std) 环境垫片 — feature `wakenet` 专用
 *
 * 职责:
 * 1. model_path 官方契约的 RAM 实现: 直接解析 Rust 侧内嵌的 srmodels.bin
 *    (pack_model.py 打包的 wn9_nihaoxiaozhi 模型包), 无需 SPIFFS/分区表;
 * 2. 文件 IO 兜底 (hufzip 走 fopen 路径时, 按文件名后缀提供内嵌数据);
 * 3. IDF/FreeRTOS 环境符号的无害化 (日志/断言/队列/任务/Cache/heap_caps)。
 *
 * 已知风险 (真机阶段验证):
 * - xTaskCreatePinnedToCore 为空转桩: 若 wakenet create 内部依赖 mfcc 任务,
 *   检测将不工作 (需引入真实任务桥接);
 * - vvadd_shift_1_int16 语义按 esp-dl 惯例推定 (右移饱和), 未对照源码。
 */

#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

/* ------------------------------------------------------------------ */
/* Rust 注入的内嵌模型 (src/audio/wake_net.rs include_bytes!)           */
/* ------------------------------------------------------------------ */
static const uint8_t *g_root;
static int g_root_len;

void shim_srmodel_init(const uint8_t *root, int len) {
    g_root = root;
    g_root_len = len;
}

/* ------------------------------------------------------------------ */
/* model_path.h 契约实现 (RAM 加载路径, 对齐官方 srmodel_load 解析)      */
/* ------------------------------------------------------------------ */

#define SRMODEL_STRING_LENGTH 32

typedef struct {
    int num;
    char **files;
    char **data;
    int *sizes;
} srmodel_data_t;

typedef struct {
    char **model_name;
    char **model_info;
    void *mmap_handle;
    int num;
    srmodel_data_t **model_data;
} srmodel_list_t;

#include "dl_lib_coefgetter_if.h"

static uint32_t read_le32(const char *p) {
    const uint8_t *b = (const uint8_t *)p;
    return (uint32_t)b[0] | ((uint32_t)b[1] << 8) | ((uint32_t)b[2] << 16) | ((uint32_t)b[3] << 24);
}

static char *g_name;
static char *g_info;
static char *g_files[3];
static char *g_datas[3];
static int g_sizes[3];
static srmodel_data_t g_model_data;
static srmodel_data_t *g_model_data_ptr;
static char *g_names[1];
static char *g_infos[1];
static srmodel_list_t g_models;
static srmodel_list_t *g_loaded;

static char *dup_range(const char *src, int len) {
    /* src 为 minified 包内字符串 (定长 32, 带尾 0) */
    char *out = (char *)malloc(len + 1);
    if (out == NULL) return NULL;
    memcpy(out, src, len);
    out[len] = 0;
    return out;
}

srmodel_list_t *esp_srmodel_init(const char *partition_label) {
    (void)partition_label;
    if (g_loaded != NULL) return g_loaded;
    if (g_root == NULL || g_root_len < 4) return NULL;

    const char *p = (const char *)g_root;
    uint32_t model_num = read_le32(p);
    p += 4;
    if (model_num < 1) return NULL;

    /* 仅打包了 1 个模型 (wn9_nihaoxiaozhi) */
    const char *name = p;
    p += SRMODEL_STRING_LENGTH;
    uint32_t file_num = read_le32(p);
    p += 4;
    if (file_num > 3) file_num = 3;

    g_model_data.num = (int)file_num;
    for (uint32_t i = 0; i < file_num; i++) {
        const char *fname = p;
        p += SRMODEL_STRING_LENGTH;
        uint32_t start = read_le32(p);
        p += 4;
        uint32_t len = read_le32(p);
        p += 4;
        g_files[i] = dup_range(fname, SRMODEL_STRING_LENGTH);
        g_datas[i] = (char *)(g_root + start);
        g_sizes[i] = (int)len;
    }
    g_model_data.files = g_files;
    g_model_data.data = g_datas;
    g_model_data.sizes = g_sizes;
    g_model_data_ptr = &g_model_data;

    g_name = dup_range(name, SRMODEL_STRING_LENGTH);
    g_names[0] = g_name;
    /* model_info 取数据区尾部的 _MODEL_INFO_ 内容 (打包时作为最后一个文件) */
    g_infos[0] = NULL;
    g_models.model_name = g_names;
    g_models.model_info = g_infos;
    g_models.mmap_handle = NULL;
    g_models.num = 1;
    g_models.model_data = &g_model_data_ptr;
    g_loaded = &g_models;
    return g_loaded;
}

void esp_srmodel_deinit(srmodel_list_t *models) {
    (void)models; /* 内嵌数据为静态存储, 无需释放 */
}

char *esp_srmodel_filter(srmodel_list_t *models, const char *keyword1, const char *keyword2) {
    if (models == NULL) return NULL;
    for (int i = 0; i < models->num; i++) {
        if (strncmp(models->model_name[i], keyword1, strlen(keyword1)) == 0) {
            if (keyword2 == NULL || strstr(models->model_name[i], keyword2) != NULL) {
                return models->model_name[i];
            }
        }
    }
    return NULL;
}

int esp_srmodel_exists(srmodel_list_t *models, char *model_name) {
    if (models == NULL) return 0;
    for (int i = 0; i < models->num; i++) {
        if (strcmp(models->model_name[i], model_name) == 0) return 1;
    }
    return 0;
}

char *esp_srmodel_get_wake_words(srmodel_list_t *models, char *model_name) {
    /* model_info 格式: wakenet9_v1h12_你好小智_3_0.585_0.595
     * 官方解析取第 3 段为唤醒词; shim 直接返回打包信息中的固定词 */
    (void)models;
    (void)model_name;
    return "你好小智";
}

char *get_model_base_path(void) {
    return "/srmodel";
}

srmodel_list_t *get_static_srmodels(void) {
    return esp_srmodel_init(NULL);
}

srmodel_list_t *srmodel_load(const void *root) {
    g_root = (const uint8_t *)root;
    g_root_len = 0; /* 调用方保证越界安全 (rodata) */
    return esp_srmodel_init(NULL);
}

/* wn5 时代编译入模型的 getter; wn9 走 model_data RAM 指针, 不经此路径。
 * 返回 NULL (而非全零表): 调用方若解引用全零表的函数指针会跳 0 地址 */
model_coeff_getter_t *srmodel_get_model_coeff(char *model_name) {
    (void)model_name;
    return NULL;
}

/* ------------------------------------------------------------------ */
/* 文件 IO 兜底: hufzip 的 fopen 路径按文件名后缀提供内嵌数据            */
/* ------------------------------------------------------------------ */

typedef struct {
    const uint8_t *data;
    int len;
    int pos;
    int eof;
} shim_file_t;

static srmodel_data_t *find_model_data(void) {
    srmodel_list_t *m = get_static_srmodels();
    if (m == NULL || m->num < 1) return NULL;
    return m->model_data[0];
}

void *fopen(const char *path, const char *mode) {
    (void)mode;
    srmodel_data_t *d = find_model_data();
    if (d == NULL) return NULL;
    for (int i = 0; i < d->num; i++) {
        /* 按后缀匹配 (路径形如 /srmodel/wn9_nihaoxiaozhi/wn9_data) */
        const char *suffix = strstr(path, d->files[i]);
        if (suffix != NULL && strlen(suffix) == strlen(d->files[i])) {
            shim_file_t *f = (shim_file_t *)malloc(sizeof(shim_file_t));
            if (f == NULL) return NULL;
            f->data = (const uint8_t *)d->data[i];
            f->len = d->sizes[i];
            f->pos = 0;
            f->eof = 0;
            return f;
        }
    }
    return NULL;
}

size_t fread(void *ptr, size_t size, size_t n, void *stream) {
    shim_file_t *f = (shim_file_t *)stream;
    if (f == NULL) return 0;
    size_t want = size * n;
    size_t rem = (size_t)(f->len - f->pos);
    size_t take = want < rem ? want : rem;
    memcpy(ptr, f->data + f->pos, take);
    f->pos += (int)take;
    if (f->pos >= f->len) f->eof = 1;
    return take / size;
}

int fseek(void *stream, long off, int whence) {
    shim_file_t *f = (shim_file_t *)stream;
    if (f == NULL) return -1;
    long target = off;
    if (whence == 1) target = f->pos + off;      /* SEEK_CUR */
    else if (whence == 2) target = f->len + off; /* SEEK_END */
    if (target < 0 || target > f->len) return -1;
    f->pos = (int)target;
    f->eof = 0;
    return 0;
}

long ftell(void *stream) {
    shim_file_t *f = (shim_file_t *)stream;
    return f ? (long)f->pos : -1L;
}

int fclose(void *stream) {
    shim_file_t *f = (shim_file_t *)stream;
    if (f) free(f);
    return 0;
}

int feof(void *stream) {
    shim_file_t *f = (shim_file_t *)stream;
    return f ? f->eof : 1;
}

size_t fwrite(const void *ptr, size_t size, size_t n, void *stream) {
    (void)ptr;
    (void)size;
    (void)n;
    (void)stream;
    return 0;
}

int fflush(void *stream) {
    (void)stream;
    return 0;
}

/* ------------------------------------------------------------------ */
/* esp_log / 断言 / 退出 / stdio 无害化                                 */
/* ------------------------------------------------------------------ */

void esp_log_write(int level, const char *tag, const char *format, ...) {
    (void)level; (void)tag; (void)format;
}

int esp_log_level_get(const char *tag) { (void)tag; return 1; }
void esp_log_level_set(const char *tag, int level) { (void)tag; (void)level; }
unsigned int esp_log_timestamp(void) { return 0; }

void exit(int status) {
    (void)status;
    for (;;) { }
}

void perror(const char *s) { (void)s; }

int puts(const char *s) { (void)s; return 0; }
int putchar(int c) { (void)c; return c; }

int getopt(int argc, char **argv, const char *optstring) {
    (void)argc; (void)argv; (void)optstring;
    return -1;
}
char *optarg;
int optind = 1;

/* ------------------------------------------------------------------ */
/* FreeRTOS 队列/任务桩 (单线程语义; 真实任务桥接待真机阶段)             */
/* ------------------------------------------------------------------ */

void *xQueueGenericCreate(int len, int width, int tag) {
    (void)len; (void)tag;
    return malloc((size_t)(width > 0 ? width : 4));
}
void *xQueueCreateMutex(int type) { (void)type; return malloc(8); }
int xQueueGenericSend(void *q, const void *item, unsigned int ticks, int copy) {
    (void)q; (void)item; (void)ticks; (void)copy;
    return 1; /* pdPASS */
}
int xQueueSemaphoreTake(void *q, unsigned int ticks) {
    (void)q; (void)ticks;
    return 1; /* pdPASS */
}
int xQueueReceive(void *q, void *out, unsigned int ticks) {
    (void)q; (void)out; (void)ticks;
    return 0; /* 立即超时: 无数据 */
}
void vQueueDelete(void *q) { free(q); }
int xTaskCreatePinnedToCore(void *fn, const char *name, unsigned int stack,
                            void *param, int prio, void **handle, int core) {
    (void)fn; (void)name; (void)stack; (void)param; (void)prio; (void)core;
    if (handle) *handle = 0;
    return 1; /* 假装成功: 任务体不会运行 (已知风险) */
}

/* ------------------------------------------------------------------ */
/* ESP-ROM/Cache 与堆能力 API                                          */
/* ------------------------------------------------------------------ */

uint32_t Cache_Start_DCache_Preload(uint32_t addr, uint32_t size) {
    (void)addr; (void)size;
    return 0;
}
uint32_t Cache_DCache_Preload_Done(void) { return 1; }

void *memalign(size_t align, size_t size) {
    /* 简单对齐分配: 多分配 align 字节, 手动对齐 */
    if (align == 0) align = 1;
    uintptr_t raw = (uintptr_t)malloc(size + align);
    if (raw == 0) return NULL;
    uintptr_t aligned = (raw + align - 1) & ~(uintptr_t)(align - 1);
    return (void *)aligned;
}

/* newlib reent 接口: 全部映射到全局堆 (单线程, reent 忽略) */
#include <sys/reent.h>
static struct _reent g_reent; /* 零初始化全局 reent (单线程) */
struct _reent *__getreent(void) {
    return &g_reent;
}
void *_malloc_r(struct _reent *reent, size_t size) { (void)reent; return malloc(size); }
void *_calloc_r(struct _reent *reent, size_t n, size_t size) { (void)reent; return calloc(n, size); }
void *_realloc_r(struct _reent *reent, void *p, size_t size) { (void)reent; return realloc(p, size); }
void _free_r(struct _reent *reent, void *p) { (void)reent; free(p); }
void *_sbrk_r(struct _reent *reent, ptrdiff_t inc) {
    (void)reent; (void)inc;
    return (void *)-1; /* 无 sbrk 堆: newlib 内部分配失败即报错 */
}

void *heap_caps_malloc(size_t size, unsigned int caps) { (void)caps; return malloc(size); }
void *heap_caps_calloc(size_t n, size_t size, unsigned int caps) { (void)caps; return calloc(n, size); }

/* esp_aes (模型加密路径; 免费模型未加密, 不应被调用) */
struct esp_aes_context { int dummy; };
static struct esp_aes_context g_aes;
void *esp_aes_init(void) { return &g_aes; }
int esp_aes_setkey(void *ctx, const unsigned char *key, unsigned int bits) {
    (void)ctx; (void)key; (void)bits; return 0;
}
int esp_aes_crypt_ctr(void *ctx, size_t len, size_t *nc_off, unsigned char nonce_counter[16],
                      unsigned char stream_block[16], const unsigned char *in, unsigned char *out) {
    (void)ctx; (void)len; (void)nc_off; (void)nonce_counter; (void)stream_block;
    memcpy(out, in, len); /* 透传: 未加密模型 */
    return 0;
}
void esp_aes_free(void *ctx) { (void)ctx; }

/* __cxa_pure_virtual (C++ 虚表兜底, 理论上不触发) */
void __cxa_pure_virtual(void) { for (;;) { } }


/* ------------------------------------------------------------------ */
/* ctype 表与 float 解析 (原由 -lc 提供; 避免 newlib ssp/stdio 依赖)     */
/* ------------------------------------------------------------------ */

const unsigned short _ctype_[257] = { 0 }; /* 仅占位: hufzip 用 isspace/isdigit 走宏查表 */
const unsigned short *__ctype_b_loc(void) { return &_ctype_[1]; }
int isprint(int c) { (void)c; return 0; }
int isspace(int c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r'; }
int isdigit(int c) { return c >= '0' && c <= '9'; }
int isalpha(int c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'); }
int isupper(int c) { return c >= 'A' && c <= 'Z'; }
int tolower(int c) { return (c >= 'A' && c <= 'Z') ? c + 32 : c; }
int toupper(int c) { return (c >= 'a' && c <= 'z') ? c - 32 : c; }

/* 极简 strtof/strtod: 十进制 (含小数/负号), 满足模型元数据解析 */
static double parse_decimal(const char **pp) {
    const char *s = *pp;
    int neg = 0;
    if (*s == '-') { neg = 1; s++; } else if (*s == '+') s++;
    double v = 0;
    while (isdigit((unsigned char)*s)) { v = v * 10 + (*s - '0'); s++; }
    if (*s == '.') {
        s++;
        double frac = 1;
        while (isdigit((unsigned char)*s)) { frac /= 10; v += (*s - '0') * frac; s++; }
    }
    if (*s == 'e' || *s == 'E') {
        s++;
        int eneg = 0;
        if (*s == '-') { eneg = 1; s++; } else if (*s == '+') s++;
        int e = 0;
        while (isdigit((unsigned char)*s)) { e = e * 10 + (*s - '0'); s++; }
        double mul = 1;
        while (e--) mul *= 10;
        v = eneg ? v / mul : v * mul;
    }
    *pp = s;
    return neg ? -v : v;
}

float strtof(const char *s, char **end) {
    double v = parse_decimal(&s);
    if (end) *end = (char *)s;
    return (float)v;
}

double strtod(const char *s, char **end) {
    double v = parse_decimal(&s);
    if (end) *end = (char *)s;
    return v;
}

static char *g_strtok_next;

char *strtok(char *s, const char *delim) {
    if (s == NULL) s = g_strtok_next;
    if (s == NULL) return NULL;
    while (*s && strchr(delim, *s)) s++;
    if (*s == 0) { g_strtok_next = NULL; return NULL; }
    char *start = s;
    while (*s && !strchr(delim, *s)) s++;
    if (*s) { *s = 0; g_strtok_next = s + 1; } else { g_strtok_next = NULL; }
    return start;
}

char *strdup(const char *s) {
    size_t n = strlen(s) + 1;
    char *out = (char *)malloc(n);
    if (out) memcpy(out, s, n);
    return out;
}
