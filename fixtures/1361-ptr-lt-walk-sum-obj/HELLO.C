int a[3];
int main(void) {
  int *p;
  int *end;
  int sum = 0;
  a[0] = 1;
  a[1] = 2;
  a[2] = 3;
  p = a;
  end = a + 3;
  while (p < end) {
    sum += *p;
    p++;
  }
  return sum;
}
