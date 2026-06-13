int g;
int f(int *p);
int main(void) {
  f(&g);
  return 0;
}
