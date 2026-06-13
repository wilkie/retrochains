int main(void) {
  char a[3];
  char *p;
  int sum;
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  p = a;
  sum = *p++;
  sum += *p++;
  sum += *p;
  return sum;
}
