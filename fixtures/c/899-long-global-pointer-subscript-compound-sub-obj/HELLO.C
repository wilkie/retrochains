long a[3];
long *p;
int main() {
  p = a;
  a[1] = 10L;
  p[1] -= 5L;
  return 0;
}
