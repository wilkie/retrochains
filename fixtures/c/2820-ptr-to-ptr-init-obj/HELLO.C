int v = 42;
int *p = &v;
int **pp = &p;
int main(void) {
  return **pp;
}
