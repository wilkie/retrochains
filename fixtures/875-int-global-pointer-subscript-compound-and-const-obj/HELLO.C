int a[3];
int *p;
int main() {
  p = a;
  p[1] &= 15;
  return 0;
}
