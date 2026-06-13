int x = 42;
int main(void) {
  int *p = &x;
  int **pp = &p;
  return **pp;
}
