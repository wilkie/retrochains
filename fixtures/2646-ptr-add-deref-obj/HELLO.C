int a[5] = { 10, 20, 30, 40, 50 };
int main(void) {
  int *p;
  p = a;
  return *(p + 2);
}
