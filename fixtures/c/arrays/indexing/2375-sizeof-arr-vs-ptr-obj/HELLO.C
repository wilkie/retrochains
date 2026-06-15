int main(void) {
  int a[5];
  int *p;
  p = a;
  return sizeof(a) - sizeof(p);
}
