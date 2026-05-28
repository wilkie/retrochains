char g;
int main(void) {
  char *p = &g;
  *p = 'A';
  return g;
}
