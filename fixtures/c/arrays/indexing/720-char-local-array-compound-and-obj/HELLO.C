int main() {
  char a[4];
  a[0] = 0;
  a[1] = 0;
  a[2] = 30;
  a[3] = 0;
  a[2] &= 15;
  return a[2];
}
