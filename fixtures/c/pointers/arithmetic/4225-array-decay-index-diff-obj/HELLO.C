int find(int *base, int target) {
  int *p;
  p = base;
  while (*p != target) {
    p++;
  }
  return (int)(p - base);
}

int main(void) {
  int a[5];
  a[0] = 11;
  a[1] = 22;
  a[2] = 33;
  a[3] = 44;
  a[4] = 55;
  return find(a, 44);
}
