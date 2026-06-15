int main()
{
  int a[4];
  int (*p)[4];
  int sum;

  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  a[3] = 40;
  p = &a;
  sum = (*p)[0] + (*p)[3];
  return sum;
}
