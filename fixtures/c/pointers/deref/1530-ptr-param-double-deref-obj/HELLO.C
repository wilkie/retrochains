int f(int *p) {
  return *p + *p;
}
int main(void) {
  int v = 7;
  return f(&v);
}
