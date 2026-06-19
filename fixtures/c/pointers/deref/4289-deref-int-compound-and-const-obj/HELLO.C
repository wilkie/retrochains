int main() {
  int x;
  int *p;
  x = 100;
  p = &x;
  *p &= 12;
  return x;
}
