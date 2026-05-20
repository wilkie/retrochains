int main(void) {
  int x = 5;
  int *p = &x;
  int **pp = &p;
  **pp += 3;
  return x;
}
