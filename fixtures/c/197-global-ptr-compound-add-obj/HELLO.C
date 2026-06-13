int g;
int *p = &g;
int main(void) {
  *p += 5;
  return g;
}
