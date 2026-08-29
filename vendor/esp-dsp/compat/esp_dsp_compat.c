/* esp-dsp aes3/ae32 汇编符号 → ANSI C 实现的桥接
 * (免 Xtensa 汇编工具链配置; 数值一致, 性能略降) */
#include "dsps_fft2r.h"
#include "dsps_fft4r.h"
#include "dsps_dotprod.h"

esp_err_t dsps_fft2r_fc32_aes3_(float *data, int N, float *w) {
    return dsps_fft2r_fc32_ansi_(data, N, w);
}

esp_err_t dsps_fft4r_fc32_aes3_(float *data, int N, float *table, int table_size) {
    return dsps_fft4r_fc32_ansi_(data, N, table, table_size);
}

esp_err_t dsps_cplx2real_fc32_ae32_(float *data, int N, float *table, int table_size) {
    return dsps_cplx2real_fc32_ansi_(data, N, table, table_size);
}

esp_err_t dsps_bit_rev4r_fc32_ae32(float *data, int N) {
    return dsps_bit_rev4r_direct_fc32_ansi(data, N);
}

esp_err_t dsps_dotprod_f32_aes3(const float *src1, const float *src2, float *dest, int len) {
    return dsps_dotprod_f32_ansi(src1, src2, dest, len);
}

esp_err_t dsps_fft4r_fc32_ae32_(float *data, int N, float *table, int table_size) {
    return dsps_fft4r_fc32_ansi_(data, N, table, table_size);
}

/* esp-dsp 支持函数 (common) */
int dsp_power_of_two(int n) {
    int p = 0;
    while ((1 << p) < n) p++;
    return p;
}

int dsp_is_power_of_two(int n) {
    return n > 0 && (n & (n - 1)) == 0;
}
