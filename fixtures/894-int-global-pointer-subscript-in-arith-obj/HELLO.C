int a[3];
int *p;
int main() {
  int x;
  p = a;
  a[1] = 7;
  x = p[1] + 5;
  return x;
}
