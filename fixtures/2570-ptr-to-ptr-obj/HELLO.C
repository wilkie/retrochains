int x = 42;
int *p = &x;
int **pp = &p;
int main(void) {
  return **pp;
}
