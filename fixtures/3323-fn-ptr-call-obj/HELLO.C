int call(int (*fp)(int), int x) {
  return fp(x);
}
