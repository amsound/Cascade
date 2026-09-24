// Comparison harness for the Rust resampler (cascade-daemon/src/audio/zita.rs, test
// `zita_compare`): prints zita-resampler 1.11.2 VResampler output for the same input.
// Build against an installed zita-resampler, e.g.
//   c++ -O2 cross/zita-compare.cc -lzita-resampler -o zita-compare
// then run `./zita-compare <ratio> [i]` (i = impulse input).
#include <cstdio>
#include <cstdlib>
#include <cmath>
#include <vector>
#include "zita-resampler/vresampler.h"
int main(int argc, char** argv) {
    double ratio = atof(argv[1]);
    int impulse = (argc > 2 && argv[2][0]=='i');
    const int HL = 32, N = 480, READAHEAD = 64;
    VResampler r;
    if (r.setup(ratio, 1, HL)) { fprintf(stderr,"setup failed\n"); return 1; }
    std::vector<float> in(N+READAHEAD,0.0f), out(N,0.0f);
    if (impulse) in[100] = 1.0f;
    else for (size_t i=0;i<in.size();i++) in[i]=sinf(i*0.01f)*0.5f;
    r.inp_data=in.data(); r.inp_count=in.size();
    r.out_data=out.data(); r.out_count=N;
    r.process();
    int produced = N - r.out_count;
    printf("%d\n", produced);
    for (int i=0;i<produced;i++) printf("%.9g\n", out[i]);
    return 0;
}
