int main(void) {
  int a = 42;
  int *p = &a;
  int **pp = &p;
  return **pp;
}
