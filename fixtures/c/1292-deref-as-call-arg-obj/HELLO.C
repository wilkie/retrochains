int f(int x) {
  return x;
}
int main(void) {
  int a = 42;
  int *p = &a;
  return f(*p);
}
