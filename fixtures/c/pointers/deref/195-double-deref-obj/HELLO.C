int g = 7;
int *q = &g;
int **p = &q;
int main(void) {
  return **p;
}
