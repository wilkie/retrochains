int g;
int main() {
  int x;
  int *p;
  g = 100;
  x = 5;
  p = &x;
  g /= *p;
  return 0;
}
