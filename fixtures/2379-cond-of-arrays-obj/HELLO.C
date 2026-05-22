int main(void) {
  int a[3];
  int b[3];
  int c;
  a[0] = 11;
  a[1] = 22;
  a[2] = 33;
  b[0] = 44;
  b[1] = 55;
  b[2] = 66;
  c = 1;
  return (c ? a : b)[1];
}
