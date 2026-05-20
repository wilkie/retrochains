int main(void) {
  int a[3];
  int r;
  a[0] = 1;
  a[1] = 2;
  a[2] = 3;
  switch (a[1]) {
    case 1: r = 10; break;
    case 2: r = 20; break;
    case 3: r = 30; break;
    default: r = 99; break;
  }
  return r;
}
