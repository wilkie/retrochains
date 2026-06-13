int two(void) { return 2; }
int main(void) {
  int (*fp)(void) = two;
  void *vp = (void *)fp;
  int (*fp2)(void) = (int (*)(void))vp;
  return fp2();
}
