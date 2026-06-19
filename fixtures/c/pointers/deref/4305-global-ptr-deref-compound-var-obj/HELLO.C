int *gp;
void fa(int v) { *gp += v; }
void fs(int v) { *gp -= v; }
void fn(int v) { *gp &= v; }
void fo(int v) { *gp |= v; }
void fx(int v) { *gp ^= v; }
int main(void) {
  int a;
  gp = &a;
  fa(1); fs(1); fn(1); fo(1); fx(1);
  return 0;
}
