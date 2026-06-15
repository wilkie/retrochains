int main(void) {
  int a[6];
  int *p = a;
  int sum = 0;
  a[0] = 1;
  a[1] = 2;
  a[2] = 3;
  a[3] = 4;
  a[4] = 5;
  a[5] = 0;
  while (*p) {
    sum += *p;
    p++;
  }
  return sum;
}
