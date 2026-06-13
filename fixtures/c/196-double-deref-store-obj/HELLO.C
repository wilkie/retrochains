int g;
int *q = &g;
int **p = &q;
int main(void) {
  **p = 42;
  return g;
}
