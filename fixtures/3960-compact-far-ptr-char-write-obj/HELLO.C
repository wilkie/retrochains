char g;
int main(void) {
  char *p = &g;
  *p = 'Y';
  return g;
}
