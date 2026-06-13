int v;
int *p = &v;
int **pp = &p;
int peek(void) {
  return **pp;
}
