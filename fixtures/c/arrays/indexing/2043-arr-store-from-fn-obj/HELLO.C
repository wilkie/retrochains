int square(int x) { return x * x; }
int main(void) {
  int a[3];
  a[0] = square(2);
  a[1] = square(3);
  a[2] = square(4);
  return a[0] + a[1] + a[2];
}
