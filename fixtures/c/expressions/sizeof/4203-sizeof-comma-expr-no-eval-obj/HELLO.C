int main(void) {
  long v;
  int n;
  n = 0;
  v = (n = 7, n + 1);
  return (int)(v + sizeof(n++, v));
}
