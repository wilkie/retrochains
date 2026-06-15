int a[3];
int *p;
int main() {
  p = a;
  a[1] = 7;
  return p[1];
}
