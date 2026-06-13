int main(void) {
  int a[5];
  int *p;
  int sum = 0;
  a[0] = 1;
  a[1] = 2;
  a[2] = 3;
  a[3] = 4;
  a[4] = 5;
  for (p = a; p < a + 5; p++) sum += *p;
  return sum;
}
