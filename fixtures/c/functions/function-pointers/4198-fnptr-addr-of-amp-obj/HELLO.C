int inc(int x) { return x + 1; }
int main(void) {
  int (*fp)(int) = &inc;
  return (*fp)(41);
}
