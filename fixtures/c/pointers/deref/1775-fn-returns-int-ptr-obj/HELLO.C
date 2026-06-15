int g = 42;
int *get_g(void) {
  return &g;
}
int main(void) {
  int *p = get_g();
  return *p;
}
