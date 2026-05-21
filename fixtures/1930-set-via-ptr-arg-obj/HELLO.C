void set(int *p, int v) { *p = v; }
int main(void) {
  int x = 0;
  set(&x, 42);
  return x;
}
