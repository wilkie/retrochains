int main(void)
{
  static int a[4] = {10, 20, 30, 40};
  int *p;
  int *end;
  int total;

  p = a;
  end = a + 4;
  total = 0;
  while (p != end) {
    total = total + *p;
    p = p + 1;
  }
  return total;
}
