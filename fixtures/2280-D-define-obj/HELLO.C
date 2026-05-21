int main(void) {
#ifdef DEBUG
  return DEBUG;
#else
  return 99;
#endif
}
