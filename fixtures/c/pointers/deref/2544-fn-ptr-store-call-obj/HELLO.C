int square(int x) { return x * x; }
int main(void) {
  int (*fp)(int);
  fp = square;
  return fp(5);
}
