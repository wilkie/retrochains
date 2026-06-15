int main(void) {
  int x;
  int n = 0;
  x = (n++, n++, n++);
  return x + n;
}
