int g;
int main() {
  int x;
  int *p;
  g = 100;
  x = 50;
  p = &x;
  g += *p;
  return 0;
}
