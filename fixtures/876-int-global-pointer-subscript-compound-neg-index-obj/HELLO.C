int a[3];
int *p;
int main() {
  int y;
  p = &a[2];
  y = 7;
  p[-1] += y;
  return 0;
}
